use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::os;

pub const MAX_SECRET_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    name: String,
    size: u64,
}

impl Entry {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

#[derive(Debug)]
pub enum Error {
    InvalidName,
    EmptyValue,
    TooLarge,
    NotFound(String),
    UnsafePermissions(String),
    Io(io::Error),
}

impl Error {
    pub fn detail(&self) -> String {
        match self {
            Self::InvalidName => "secret name must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$".into(),
            Self::EmptyValue => "secret value must not be empty".into(),
            Self::TooLarge => format!("secret exceeds maximum size ({MAX_SECRET_BYTES} bytes)"),
            Self::NotFound(name) => format!("secret '{name}' does not exist"),
            Self::UnsafePermissions(msg) => msg.clone(),
            Self::Io(err) => err.to_string(),
        }
    }
}

pub fn validate_name<S: AsRef<str>>(name: S) -> Result<(), Error> {
    let s = name.as_ref();
    let bytes = s.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(Error::InvalidName);
    }
    Ok(())
}

pub fn zeroize(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

pub struct Store {
    root: PathBuf,
    owner_uid: u32,
    policy: DirectoryPolicy,
}

#[derive(PartialEq, Eq)]
enum DirectoryPolicy {
    CreateUnderHome,
    Existing,
}

impl Store {
    pub fn for_current_user() -> Result<Self, Error> {
        let uid = os::current_euid();
        let user = os::user_by_uid(uid).map_err(|e| Error::UnsafePermissions(e.detail()))?;
        Ok(Self::from_home(user.home(), uid))
    }

    pub fn from_home<P: AsRef<Path>>(home: P, owner_uid: u32) -> Self {
        Self {
            root: home.as_ref().join(".config/boreal/secrets"),
            owner_uid,
            policy: DirectoryPolicy::CreateUnderHome,
        }
    }

    /// Open an existing private secrets directory without creating or changing it.
    /// The caller must supply a path under trusted parent directories.
    pub fn from_existing_dir<P: AsRef<Path>>(root: P, owner_uid: u32) -> Result<Self, Error> {
        let root = root.as_ref();
        if !root.is_absolute() || root.components().any(|c| c == Component::ParentDir) {
            return Err(Error::UnsafePermissions(
                "secret directory path must be absolute and contain no '..' components".into(),
            ));
        }
        let store = Self {
            // A trailing '/' or '/.' would otherwise hide a final symlink.
            root: root.components().collect(),
            owner_uid,
            policy: DirectoryPolicy::Existing,
        };
        store.verify_root_dir()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ensure_dirs(&self) -> Result<(), Error> {
        if self.policy == DirectoryPolicy::Existing {
            return self.verify_root_dir();
        }
        let boreal_dir = self
            .root
            .parent()
            .ok_or_else(|| Error::UnsafePermissions("invalid secrets path".into()))?;
        let config_dir = boreal_dir
            .parent()
            .ok_or_else(|| Error::UnsafePermissions("invalid secrets path".into()))?;
        for ancestor in [config_dir, boreal_dir] {
            match DirBuilder::new().mode(0o700).create(ancestor) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(Error::Io(e)),
            }
            let meta = fs::symlink_metadata(ancestor).map_err(Error::Io)?;
            if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != self.owner_uid {
                return Err(Error::UnsafePermissions(format!(
                    "{} must be a non-symlink directory owned by uid {}",
                    ancestor.display(),
                    self.owner_uid
                )));
            }
            if meta.mode() & 0o022 != 0 {
                return Err(Error::UnsafePermissions(format!(
                    "{} must not be group or world writable",
                    ancestor.display()
                )));
            }
        }
        match DirBuilder::new().mode(0o700).create(&self.root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::Io(e)),
        }
        self.verify_root_dir()
    }

    fn verify_root_dir(&self) -> Result<(), Error> {
        let meta = fs::symlink_metadata(&self.root).map_err(Error::Io)?;
        if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != self.owner_uid {
            return Err(Error::UnsafePermissions(format!(
                "{} must be a non-symlink directory owned by uid {}",
                self.root.display(),
                self.owner_uid
            )));
        }
        let mode = meta.mode() & 0o7777;
        let mask = if self.policy == DirectoryPolicy::Existing {
            0o7777
        } else {
            0o777
        };
        if mode & mask != 0o700 {
            return Err(Error::UnsafePermissions(format!(
                "{} must have mode 0700 (found {mode:04o})",
                self.root.display()
            )));
        }
        Ok(())
    }

    fn root_present(&self) -> Result<bool, Error> {
        match self.verify_root_dir() {
            Ok(()) => Ok(true),
            Err(Error::Io(e))
                if e.kind() == io::ErrorKind::NotFound
                    && self.policy == DirectoryPolicy::CreateUnderHome =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    pub fn exists<S: AsRef<str>>(&self, name: S) -> Result<bool, Error> {
        let name = name.as_ref();
        validate_name(name)?;
        if self.policy == DirectoryPolicy::CreateUnderHome && !self.root.exists() {
            return Ok(false);
        }
        self.verify_root_dir()?;
        let path = self.root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(Error::UnsafePermissions(format!(
                        "{} is not a regular file",
                        path.display()
                    )));
                }
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(e)),
        }
    }

    pub fn put<S: AsRef<str>>(&self, name: S, value: &[u8]) -> Result<usize, Error> {
        let name = name.as_ref();
        validate_name(name)?;
        if value.is_empty() {
            return Err(Error::EmptyValue);
        }
        if value.len() > MAX_SECRET_BYTES {
            return Err(Error::TooLarge);
        }
        self.ensure_dirs()?;
        let target = self.root.join(name);
        if let Ok(meta) = fs::symlink_metadata(&target)
            && (meta.file_type().is_symlink() || !meta.is_file())
        {
            return Err(Error::UnsafePermissions(format!(
                "{} is not a regular file",
                target.display()
            )));
        }

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut tmp_path = None;
        let mut tmp_file = None;
        for seq in 0..256u32 {
            let candidate = self
                .root
                .join(format!(".tmp.{}.{stamp}.{seq}", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(os::O_NOFOLLOW)
                .open(&candidate)
            {
                Ok(f) => {
                    tmp_path = Some(candidate);
                    tmp_file = Some(f);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::Io(e)),
            }
        }
        let (tmp_path, mut file) = tmp_path
            .zip(tmp_file)
            .ok_or_else(|| Error::Io(io::Error::other("failed to allocate temporary secret")))?;

        let write_res = (|| -> Result<(), io::Error> {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(value)?;
            file.sync_all()?;
            fs::rename(&tmp_path, &target)?;
            File::open(&self.root)?.sync_all()
        })();

        if let Err(err) = write_res {
            let _ = fs::remove_file(&tmp_path);
            return Err(Error::Io(err));
        }
        Ok(value.len())
    }

    pub fn list(&self) -> Result<Vec<Entry>, Error> {
        if !self.root_present()? {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.root).map_err(Error::Io)? {
            let item = item.map_err(Error::Io)?;
            let os_name = item.file_name();
            let Some(name) = os_name.to_str() else {
                continue;
            };
            if name.starts_with(".tmp.") {
                continue;
            }
            validate_name(name)?;
            let meta = fs::symlink_metadata(item.path()).map_err(Error::Io)?;
            if meta.file_type().is_symlink() || !meta.is_file() || meta.uid() != self.owner_uid {
                return Err(Error::UnsafePermissions(format!(
                    "secret '{}' must be a regular file owned by uid {}",
                    name, self.owner_uid
                )));
            }
            if meta.mode() & 0o777 != 0o600 {
                return Err(Error::UnsafePermissions(format!(
                    "secret '{name}' must have mode 0600 (found {:04o})",
                    meta.mode() & 0o7777
                )));
            }
            entries.push(Entry {
                name: name.to_owned(),
                size: meta.len(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    pub fn remove<S: AsRef<str>>(&self, name: S) -> Result<(), Error> {
        let name = name.as_ref();
        validate_name(name)?;
        if !self.root_present()? {
            return Err(Error::NotFound(name.to_owned()));
        }
        let path = self.root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() || meta.uid() != self.owner_uid
                {
                    return Err(Error::UnsafePermissions(format!(
                        "secret '{name}' is not a regular file owned by uid {}",
                        self.owner_uid
                    )));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::NotFound(name.to_owned()));
            }
            Err(e) => return Err(Error::Io(e)),
        }
        fs::remove_file(&path).map_err(Error::Io)?;
        File::open(&self.root)
            .and_then(|f| f.sync_all())
            .map_err(Error::Io)
    }

    pub fn read_bytes<S: AsRef<str>>(&self, name: S) -> Result<Vec<u8>, Error> {
        let name = name.as_ref();
        validate_name(name)?;
        if !self.root_present()? {
            return Err(Error::NotFound(name.to_owned()));
        }
        let path = self.root.join(name);
        let bytes = os::read_owned_file(&path, Some(self.owner_uid), 0o177, MAX_SECRET_BYTES)
            .map_err(|e| match e {
                os::Error::Io(io_err) if io_err.kind() == io::ErrorKind::NotFound => {
                    Error::NotFound(name.to_owned())
                }
                os::Error::Io(io_err) => Error::Io(io_err),
                other => Error::UnsafePermissions(other.detail()),
            })?;
        let meta = fs::symlink_metadata(&path).map_err(Error::Io)?;
        if meta.mode() & 0o777 != 0o600 {
            return Err(Error::UnsafePermissions(format!(
                "secret '{name}' must have mode 0600"
            )));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "boreal-secret-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    #[test]
    fn test_secret_lifecycle_modes_and_name_validation() {
        let home = test_dir();
        let uid = os::current_euid();
        let store = Store::from_home(&home, uid);

        for bad in ["", ".hidden", "../x", "a/b", "a b", "a$b"] {
            assert!(matches!(store.put(bad, b"val"), Err(Error::InvalidName)));
        }
        assert!(matches!(store.put("tok", b""), Err(Error::EmptyValue)));

        assert_eq!(store.put("api_token", b"dummy-test-bytes").unwrap(), 16);
        assert_eq!(
            fs::symlink_metadata(store.root()).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::symlink_metadata(store.root().join("api_token"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );

        let list = store.list().unwrap();
        assert_eq!(
            list,
            vec![Entry {
                name: "api_token".into(),
                size: 16
            }]
        );
        assert_eq!(store.read_bytes("api_token").unwrap(), b"dummy-test-bytes");

        fs::set_permissions(
            store.root().join("api_token"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert!(matches!(
            store.read_bytes("api_token"),
            Err(Error::UnsafePermissions(_))
        ));
        fs::set_permissions(
            store.root().join("api_token"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        store.remove("api_token").unwrap();
        assert!(store.list().unwrap().is_empty());

        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn existing_store_requires_an_owned_private_directory() {
        let parent = test_dir();
        let root = parent.join("secrets");
        let uid = os::current_euid();
        assert!(matches!(
            Store::from_existing_dir(&root, uid),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound
        ));
        assert!(!root.exists());
        assert!(matches!(
            Store::from_existing_dir("relative", uid),
            Err(Error::UnsafePermissions(_))
        ));

        fs::write(&root, b"fixture").unwrap();
        assert!(matches!(
            Store::from_existing_dir(&root, uid),
            Err(Error::UnsafePermissions(_))
        ));
        fs::remove_file(&root).unwrap();
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        assert!(matches!(
            Store::from_existing_dir(&root, uid ^ 1),
            Err(Error::UnsafePermissions(_))
        ));

        let link = parent.join("linked");
        symlink(&root, &link).unwrap();
        for path in [&link, &link.join("."), &link.join("")] {
            assert!(matches!(
                Store::from_existing_dir(path, uid),
                Err(Error::UnsafePermissions(_))
            ));
        }

        for mode in [0o500, 0o750, 0o1700] {
            fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
            assert!(matches!(
                Store::from_existing_dir(&root, uid),
                Err(Error::UnsafePermissions(_))
            ));
            assert_eq!(fs::metadata(&root).unwrap().mode() & 0o7777, mode);
        }
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn existing_store_never_recreates_a_removed_directory() {
        let parent = test_dir();
        let root = parent.join("secrets");
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let store = Store::from_existing_dir(&root, os::current_euid()).unwrap();
        store.put("item", b"fixture").unwrap();
        assert_eq!(store.list().unwrap()[0].name(), "item");
        assert!(store.exists("item").unwrap());
        assert_eq!(store.read_bytes("item").unwrap(), b"fixture");
        store.remove("item").unwrap();
        fs::remove_dir(&root).unwrap();

        for error in [
            store.put("item", b"fixture").unwrap_err(),
            store.exists("item").unwrap_err(),
            store.list().unwrap_err(),
            store.read_bytes("item").unwrap_err(),
            store.remove("item").unwrap_err(),
        ] {
            assert!(matches!(error, Error::Io(e) if e.kind() == io::ErrorKind::NotFound));
        }
        assert!(!root.exists());
        fs::remove_dir(parent).unwrap();
    }
}
