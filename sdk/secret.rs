//! Read user-managed values inside a dedicated vault activity.

use std::{
    ffi::CStr,
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub type Result<T> = std::result::Result<T, crate::Error>;

const MAX_SECRET_BYTES: usize = 64 * 1024;

#[cfg(target_arch = "aarch64")]
const O_NOFOLLOW: i32 = 0o100000;
#[cfg(not(target_arch = "aarch64"))]
const O_NOFOLLOW: i32 = 0o400000;

/// Reads a user-managed value as bytes. Never include the value in Temporal
/// payloads, failures, heartbeats, or logs.
pub fn read_bytes(name: &str) -> Result<Vec<u8>> {
    let (uid, home) = current_user_home()?;
    read_from_home(&home, uid, name)
}

/// Reads a user-managed UTF-8 value. Invalid UTF-8 errors omit the source bytes.
pub fn read(name: &str) -> Result<String> {
    String::from_utf8(read_bytes(name)?).map_err(|err| err.utf8_error().into())
}

fn current_user_home() -> Result<(u32, PathBuf)> {
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
    unsafe extern "C" {
        fn geteuid() -> u32;
        fn getpwuid_r(
            uid: u32,
            pwd: *mut Passwd,
            buf: *mut std::ffi::c_char,
            buflen: usize,
            result: *mut *mut Passwd,
        ) -> i32;
    }
    let uid = unsafe { geteuid() };
    let mut buf = vec![0u8; 16384];
    let mut pwd = std::mem::MaybeUninit::<Passwd>::zeroed();
    let mut out: *mut Passwd = std::ptr::null_mut();
    let rc = unsafe {
        getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            &raw mut out,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc).into());
    }
    if out.is_null() {
        return Err(format!("no passwd entry for uid {uid}").into());
    }
    let pwd = unsafe { pwd.assume_init() };
    if pwd.pw_dir.is_null() {
        return Err("missing home directory in passwd entry".into());
    }
    let home = unsafe { CStr::from_ptr(pwd.pw_dir) }.to_str()?;
    if home.is_empty() {
        return Err("empty home directory in passwd entry".into());
    }
    Ok((uid, PathBuf::from(home)))
}

fn read_from_home(home: &Path, owner_uid: u32, name: &str) -> Result<Vec<u8>> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("invalid secret name".into());
    }
    let dir = home.join(".config/boreal/secrets");
    let dir_meta = fs::symlink_metadata(&dir)?;
    if dir_meta.file_type().is_symlink()
        || !dir_meta.is_dir()
        || dir_meta.uid() != owner_uid
        || dir_meta.mode() & 0o777 != 0o700
    {
        return Err("secrets directory must be a non-symlink 0700 directory owned by user".into());
    }
    let path = dir.join(name);
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(&path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != owner_uid || meta.mode() & 0o777 != 0o600 {
        return Err("secret file must be a regular 0600 file owned by user".into());
    }
    if meta.len() > MAX_SECRET_BYTES as u64 {
        return Err("secret exceeds maximum permitted size".into());
    }
    let mut data = Vec::with_capacity(meta.len() as usize);
    File::by_ref(&mut file)
        .take((MAX_SECRET_BYTES + 1) as u64)
        .read_to_end(&mut data)?;
    if data.len() > MAX_SECRET_BYTES {
        return Err("secret exceeds maximum permitted size".into());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn test_read_enforces_directory_and_file_modes() {
        let (uid, _) = current_user_home().unwrap();
        let home = std::env::temp_dir().join(format!("boreal-sdk-secret-{}", std::process::id()));
        fs::create_dir(&home).unwrap();
        let secrets = home.join(".config/boreal/secrets");
        fs::create_dir_all(&secrets).unwrap();
        fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();

        let file = secrets.join("token");
        fs::write(&file, b"test-only-placeholder").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            read_from_home(&home, uid, "token").unwrap(),
            b"test-only-placeholder"
        );
        assert!(read_from_home(&home, uid, "../token").is_err());

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_from_home(&home, uid, "token").is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        fs::set_permissions(&secrets, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(read_from_home(&home, uid, "token").is_err());

        fs::remove_dir_all(&home).unwrap();
    }
}
