use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[cfg(target_arch = "aarch64")]
pub const O_NOFOLLOW: i32 = 0o100000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_NOFOLLOW: i32 = 0o400000;

const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17;

#[repr(C)]
struct Ucred {
    pid: i32,
    uid: u32,
    gid: u32,
}

#[repr(C)]
struct Passwd {
    pw_name: *mut std::ffi::c_char,
    pw_passwd: *mut std::ffi::c_char,
    pw_uid: u32,
    pw_gid: u32,
    pw_gecos: *mut std::ffi::c_char,
    pw_dir: *mut std::ffi::c_char,
    pw_shell: *mut std::ffi::c_char,
}

#[repr(C)]
struct Group {
    gr_name: *mut std::ffi::c_char,
    gr_passwd: *mut std::ffi::c_char,
    gr_gid: u32,
    gr_mem: *mut *mut std::ffi::c_char,
}

unsafe extern "C" {
    fn getuid() -> u32;
    fn geteuid() -> u32;
    fn getsockopt(
        sockfd: i32,
        level: i32,
        optname: i32,
        optval: *mut std::ffi::c_void,
        optlen: *mut u32,
    ) -> i32;
    fn getpwuid_r(
        uid: u32,
        pwd: *mut Passwd,
        buf: *mut std::ffi::c_char,
        buflen: usize,
        result: *mut *mut Passwd,
    ) -> i32;
    fn getpwnam_r(
        name: *const std::ffi::c_char,
        pwd: *mut Passwd,
        buf: *mut std::ffi::c_char,
        buflen: usize,
        result: *mut *mut Passwd,
    ) -> i32;
    fn getgrnam_r(
        name: *const std::ffi::c_char,
        grp: *mut Group,
        buf: *mut std::ffi::c_char,
        buflen: usize,
        result: *mut *mut Group,
    ) -> i32;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    name: String,
    uid: u32,
    gid: u32,
    home: PathBuf,
}

impl User {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    NotFound(String),
    InvalidIdentity(String),
    UnsafePath(String),
}

impl Error {
    pub fn detail(&self) -> String {
        match self {
            Self::Io(err) => err.to_string(),
            Self::NotFound(msg) | Self::InvalidIdentity(msg) | Self::UnsafePath(msg) => msg.clone(),
        }
    }
}

pub fn current_uid() -> u32 {
    unsafe { getuid() }
}

pub fn current_euid() -> u32 {
    unsafe { geteuid() }
}

pub fn sanitize_hostname<S: AsRef<str>>(raw: S) -> String {
    let cleaned: String = raw
        .as_ref()
        .trim()
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .take(64)
        .collect();
    if cleaned.is_empty() || cleaned == "(none)" {
        String::from("boreal")
    } else {
        cleaned
    }
}

pub fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| fs::read_to_string("/etc/hostname"))
        .map(sanitize_hostname)
        .unwrap_or_else(|_| String::from("boreal"))
}

pub fn peer_uid(stream: &UnixStream) -> Result<u32, Error> {
    let mut cred = Ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = std::mem::size_of::<Ucred>() as u32;
    let rc = unsafe {
        getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }
    if len as usize != std::mem::size_of::<Ucred>() || cred.uid == u32::MAX {
        return Err(Error::InvalidIdentity(
            "missing peer credentials on Unix socket".into(),
        ));
    }
    Ok(cred.uid)
}

pub fn user_by_uid(uid: u32) -> Result<User, Error> {
    let mut buf = vec![0u8; 16384];
    let mut pwd = std::mem::MaybeUninit::<Passwd>::zeroed();
    let mut result: *mut Passwd = std::ptr::null_mut();
    let rc = unsafe {
        getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &raw mut result,
        )
    };
    if rc != 0 {
        return Err(Error::Io(io::Error::from_raw_os_error(rc)));
    }
    if result.is_null() {
        return Err(Error::NotFound(format!("no passwd entry for uid {uid}")));
    }
    let pwd = unsafe { pwd.assume_init() };
    decode_passwd(&pwd)
}

pub fn user_by_name<S: AsRef<str>>(name: S) -> Result<User, Error> {
    let name_str = name.as_ref();
    let c_name = CString::new(name_str)
        .map_err(|_| Error::InvalidIdentity("invalid NUL in username".into()))?;
    let mut buf = vec![0u8; 16384];
    let mut pwd = std::mem::MaybeUninit::<Passwd>::zeroed();
    let mut result: *mut Passwd = std::ptr::null_mut();
    let rc = unsafe {
        getpwnam_r(
            c_name.as_ptr(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &raw mut result,
        )
    };
    if rc != 0 {
        return Err(Error::Io(io::Error::from_raw_os_error(rc)));
    }
    if result.is_null() {
        return Err(Error::NotFound(format!(
            "no passwd entry for user {name_str}"
        )));
    }
    let pwd = unsafe { pwd.assume_init() };
    decode_passwd(&pwd)
}

pub fn group_gid_by_name<S: AsRef<str>>(name: S) -> Result<u32, Error> {
    let name_str = name.as_ref();
    let c_name = CString::new(name_str)
        .map_err(|_| Error::InvalidIdentity("invalid NUL in group name".into()))?;
    let mut buf = vec![0u8; 16384];
    let mut grp = std::mem::MaybeUninit::<Group>::zeroed();
    let mut result: *mut Group = std::ptr::null_mut();
    let rc = unsafe {
        getgrnam_r(
            c_name.as_ptr(),
            grp.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &raw mut result,
        )
    };
    if rc != 0 {
        return Err(Error::Io(io::Error::from_raw_os_error(rc)));
    }
    if result.is_null() {
        return Err(Error::NotFound(format!("no group entry for {name_str}")));
    }
    let grp = unsafe { grp.assume_init() };
    Ok(grp.gr_gid)
}

fn decode_passwd(pwd: &Passwd) -> Result<User, Error> {
    if pwd.pw_name.is_null() || pwd.pw_dir.is_null() {
        return Err(Error::InvalidIdentity("incomplete passwd record".into()));
    }
    let name = unsafe { CStr::from_ptr(pwd.pw_name) }
        .to_str()
        .map_err(|_| Error::InvalidIdentity("non-UTF-8 username".into()))?
        .to_owned();
    let home_str = unsafe { CStr::from_ptr(pwd.pw_dir) }
        .to_str()
        .map_err(|_| Error::InvalidIdentity("non-UTF-8 home path".into()))?;
    if name.is_empty() || home_str.is_empty() {
        return Err(Error::InvalidIdentity("empty username or home path".into()));
    }
    Ok(User {
        name,
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
        home: PathBuf::from(home_str),
    })
}

pub fn verify_dir_owner_and_mode(
    path: &Path,
    expected_uid: Option<u32>,
    forbidden_mode_bits: u32,
) -> Result<(), Error> {
    let meta = fs::symlink_metadata(path).map_err(Error::Io)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Error::UnsafePath(format!(
            "{} must be a non-symlink directory",
            path.display()
        )));
    }
    if let Some(uid) = expected_uid
        && meta.uid() != uid
    {
        return Err(Error::UnsafePath(format!(
            "{} is owned by uid {}, expected {uid}",
            path.display(),
            meta.uid()
        )));
    }
    if meta.mode() & forbidden_mode_bits != 0 {
        return Err(Error::UnsafePath(format!(
            "{} has unsafe mode {:04o}",
            path.display(),
            meta.mode() & 0o7777
        )));
    }
    Ok(())
}

pub fn read_owned_file(
    path: &Path,
    expected_uid: Option<u32>,
    forbidden_mode_bits: u32,
    max_bytes: usize,
) -> Result<Vec<u8>, Error> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(Error::Io)?;
    let meta = file.metadata().map_err(Error::Io)?;
    if !meta.is_file() {
        return Err(Error::UnsafePath(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    if let Some(uid) = expected_uid
        && meta.uid() != uid
    {
        return Err(Error::UnsafePath(format!(
            "{} is owned by uid {}, expected {uid}",
            path.display(),
            meta.uid()
        )));
    }
    if meta.mode() & forbidden_mode_bits != 0 {
        return Err(Error::UnsafePath(format!(
            "{} has unsafe mode {:04o}",
            path.display(),
            meta.mode() & 0o7777
        )));
    }
    if meta.len() > max_bytes as u64 {
        return Err(Error::UnsafePath(format!(
            "{} exceeds maximum size of {max_bytes} bytes",
            path.display()
        )));
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    File::by_ref(&mut file)
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(Error::Io)?;
    if buf.len() > max_bytes {
        return Err(Error::UnsafePath(format!(
            "{} exceeds maximum size of {max_bytes} bytes",
            path.display()
        )));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};

    #[test]
    fn test_current_user_and_peer_uid_resolution() {
        let uid = current_euid();
        let me = user_by_uid(uid).unwrap();
        assert_eq!(me.uid(), uid);
        assert!(!me.name().is_empty());

        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), uid);
        assert!(!hostname().is_empty());

        let host_cases = [
            ("demo-lab\n", "demo-lab"),
            ("  lab-01.internal  ", "lab-01.internal"),
            ("bad\x1b[31mhost\x07", "bad[31mhost"),
            ("", "boreal"),
            (" \t\n ", "boreal"),
            ("(none)\n", "boreal"),
        ];
        for (input, expected) in host_cases {
            assert_eq!(sanitize_hostname(input), expected, "{input:?}");
        }
    }

    #[test]
    fn test_read_owned_file_rejects_symlinks_and_loose_modes() {
        let dir = std::env::temp_dir().join(format!(
            "boreal-os-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let good = dir.join("good.yaml");
        fs::write(&good, b"ok").unwrap();
        fs::set_permissions(&good, fs::Permissions::from_mode(0o600)).unwrap();

        let uid = current_euid();
        assert!(verify_dir_owner_and_mode(&dir, Some(uid), 0o077).is_ok());
        assert_eq!(
            read_owned_file(&good, Some(uid), 0o077, 1024).unwrap(),
            b"ok"
        );

        fs::set_permissions(&good, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_owned_file(&good, Some(uid), 0o077, 1024),
            Err(Error::UnsafePath(_))
        ));
        fs::set_permissions(&good, fs::Permissions::from_mode(0o600)).unwrap();

        let link = dir.join("link.yaml");
        symlink(&good, &link).unwrap();
        assert!(read_owned_file(&link, Some(uid), 0o077, 1024).is_err());

        fs::remove_file(&link).unwrap();
        fs::remove_file(&good).unwrap();
        fs::remove_dir(&dir).unwrap();
    }
}
