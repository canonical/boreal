use std::fs::{self, File};
use std::io;
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;

use boreal_common::os;

const CLONE_NEWNET: i32 = 0x4000_0000;
const PR_SET_DUMPABLE: i32 = 4;
const PR_SET_NO_NEW_PRIVS: i32 = 38;

unsafe extern "C" {
    fn setns(fd: i32, nstype: i32) -> i32;
    fn chown(path: *const std::ffi::c_char, owner: u32, group: u32) -> i32;
    fn setgroups(size: usize, list: *const u32) -> i32;
    fn setresgid(rgid: u32, egid: u32, sgid: u32) -> i32;
    fn setresuid(ruid: u32, euid: u32, suid: u32) -> i32;
    fn prctl(option: i32, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> i32;
}

pub fn enter_netns(path: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("open netns {}: {e}", path.display()))?;
    let rc = unsafe { setns(file.as_raw_fd(), CLONE_NEWNET) };
    if rc != 0 {
        return Err(format!(
            "setns({}): {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

pub fn bind_tcp_in_netns(netns_path: &Path, addr: &str) -> Result<TcpListener, String> {
    let original =
        File::open("/proc/self/ns/net").map_err(|e| format!("open /proc/self/ns/net: {e}"))?;
    enter_netns(netns_path)?;
    let bind_res = TcpListener::bind(addr)
        .and_then(|l| {
            l.set_nonblocking(true)?;
            Ok(l)
        })
        .map_err(|e| format!("bind {addr} in {}: {e}", netns_path.display()));
    let restore_rc = unsafe { setns(original.as_raw_fd(), CLONE_NEWNET) };
    if restore_rc != 0 {
        return Err(format!(
            "restore original netns: {}",
            io::Error::last_os_error()
        ));
    }
    bind_res
}

pub fn bind_unix_socket(
    path: &Path,
    owner_uid: u32,
    owner_gid: u32,
    mode: u32,
) -> Result<UnixListener, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create socket parent {}: {e}", parent.display()))?;
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove stale socket {}: {e}", path.display())),
    }
    let listener = UnixListener::bind(path)
        .map_err(|e| format!("bind unix socket {}: {e}", path.display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set nonblocking {}: {e}", path.display()))?;

    let c_path = std::ffi::CString::new(
        path.to_str()
            .ok_or_else(|| format!("non-UTF-8 socket path {}", path.display()))?,
    )
    .map_err(|_| "NUL in socket path".to_owned())?;
    if unsafe { chown(c_path.as_ptr(), owner_uid, owner_gid) } != 0 {
        return Err(format!(
            "chown {}: {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(listener)
}

pub fn drop_privileges(user_name: &str, extra_groups: &[&str]) -> Result<(), String> {
    let user = os::user_by_name(user_name).map_err(|e| e.detail())?;
    let mut gids = Vec::with_capacity(extra_groups.len());
    for gname in extra_groups {
        let gid = os::group_gid_by_name(gname).map_err(|e| e.detail())?;
        gids.push(gid);
    }
    let ptr = if gids.is_empty() {
        std::ptr::null()
    } else {
        gids.as_ptr()
    };
    if unsafe { setgroups(gids.len(), ptr) } != 0 {
        return Err(format!("setgroups: {}", io::Error::last_os_error()));
    }
    if unsafe { setresgid(user.gid(), user.gid(), user.gid()) } != 0 {
        return Err(format!("setresgid: {}", io::Error::last_os_error()));
    }
    if unsafe { setresuid(user.uid(), user.uid(), user.uid()) } != 0 {
        return Err(format!("setresuid: {}", io::Error::last_os_error()));
    }
    if unsafe { prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(format!("prctl(DUMPABLE): {}", io::Error::last_os_error()));
    }
    if unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(format!(
            "prctl(NO_NEW_PRIVS): {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}
