use std::{
    env,
    error::Error,
    ffi::CStr,
    fs::{self, File, OpenOptions},
    future::Future,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub use temporalio_client;
pub use temporalio_common;
pub use temporalio_macros;
pub use temporalio_sdk;
pub use temporalio_workflow;

use temporalio_client::{
    Client, ClientOptions, Connection, envconfig::LoadClientConfigProfileOptions,
};
use temporalio_sdk::{Runtime, Worker, WorkerOptions};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

#[cfg(target_arch = "aarch64")]
const O_NOFOLLOW: i32 = 0o100000;
#[cfg(not(target_arch = "aarch64"))]
const O_NOFOLLOW: i32 = 0o400000;

#[cfg(unix)]
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

fn read_secret_from_home(home: &Path, owner_uid: u32, name: &str) -> Result<Vec<u8>> {
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

/// Reads a user-managed secret from `~/.config/boreal/secrets/<name>` inside an activity.
/// Never pass the returned bytes into workflow inputs, results, errors, or logs.
#[cfg(unix)]
pub fn read_secret_bytes(name: &str) -> Result<Vec<u8>> {
    let (uid, home) = current_user_home()?;
    read_secret_from_home(&home, uid, name)
}

#[cfg(unix)]
pub fn read_secret(name: &str) -> Result<String> {
    let bytes = read_secret_bytes(name)?;
    Ok(String::from_utf8(bytes)?)
}

fn resolve_queue(override_queue: Option<String>, default_queue: &str) -> Result<String> {
    let queue = override_queue.unwrap_or_else(|| default_queue.to_owned());
    if queue.trim().is_empty() {
        return Err("task queue must not be empty".into());
    }
    Ok(queue)
}

pub fn task_queue(default_queue: &str) -> Result<String> {
    let override_queue = match env::var("LAB_TASK_QUEUE") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(err) => return Err(err.into()),
    };
    resolve_queue(override_queue, default_queue)
}

/// Connects using Temporal's standard TEMPORAL_ADDRESS, TEMPORAL_NAMESPACE, and TLS settings.
pub async fn connect() -> Result<Client> {
    let (connection_options, client_options) =
        ClientOptions::load_from_config(LoadClientConfigProfileOptions::default())?;
    let connection = Connection::connect(connection_options).await?;
    Ok(Client::new(connection, client_options)?)
}

/// Continues polling the worker future after requesting shutdown so in-flight work can drain.
async fn run_until_shutdown<F, S, H, T>(worker: F, signal: S, shutdown: H) -> T
where
    F: Future<Output = T>,
    S: Future<Output = ()>,
    H: FnOnce(),
{
    tokio::pin!(worker);
    tokio::select! {
        result = &mut worker => result,
        () = signal => {
            shutdown();
            worker.await
        }
    }
}

#[cfg(unix)]
pub async fn run_worker<F>(default_queue: &str, build_options: F) -> Result<()>
where
    F: FnOnce(String) -> Result<WorkerOptions>,
{
    use tokio::signal::unix::{SignalKind, signal};

    // Install signal handlers before connecting; connection setup can itself block.
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let stop = async {
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
    };
    tokio::pin!(stop);

    let queue = task_queue(default_queue)?;
    let options = build_options(queue)?;
    let runtime = Runtime::from_current_tokio(Default::default())?;
    let client = tokio::select! {
        result = connect() => result?,
        () = &mut stop => return Ok(()),
    };
    let mut worker = Worker::new(&runtime, client, options)?;
    let shutdown = worker.shutdown_handle();
    run_until_shutdown(worker.run(), stop, shutdown).await?;
    Ok(())
}

#[cfg(not(unix))]
pub async fn run_worker<F>(_default_queue: &str, _build_options: F) -> Result<()>
where
    F: FnOnce(String) -> Result<WorkerOptions>,
{
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this worker requires Unix signals",
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, future};

    #[test]
    fn test_task_queue_uses_default_or_validates_override() {
        assert_eq!(resolve_queue(None, "lab").unwrap(), "lab");
        assert_eq!(
            resolve_queue(Some("alice-greet".into()), "lab").unwrap(),
            "alice-greet"
        );
        assert!(resolve_queue(Some("  ".into()), "lab").is_err());
        assert!(resolve_queue(None, " ").is_err());
    }

    #[tokio::test]
    async fn test_shutdown_requests_drain_and_awaits_worker_completion() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let drained = Cell::new(false);
        let result =
            run_until_shutdown(async { receiver.await.unwrap() }, future::ready(()), || {
                drained.set(true);
                sender.send(42).unwrap();
            })
            .await;
        assert!(drained.get());
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_worker_error_is_not_hidden_by_shutdown() {
        let result = run_until_shutdown(
            future::ready(Err::<(), _>("worker error")),
            future::pending(),
            || panic!("unexpected shutdown"),
        )
        .await;
        assert_eq!(result, Err("worker error"));
    }

    #[test]
    fn test_read_secret_enforces_directory_and_file_modes() {
        use std::os::unix::fs::PermissionsExt;

        let (uid, _) = current_user_home().unwrap();
        let home = env::temp_dir().join(format!("boreal-worker-secret-{}", std::process::id()));
        let secrets = home.join(".config/boreal/secrets");
        fs::create_dir_all(&secrets).unwrap();
        fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();

        let file = secrets.join("token");
        fs::write(&file, b"test-only-placeholder").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            read_secret_from_home(&home, uid, "token").unwrap(),
            b"test-only-placeholder"
        );
        assert!(read_secret_from_home(&home, uid, "../token").is_err());

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_secret_from_home(&home, uid, "token").is_err());

        let _ = fs::remove_dir_all(&home);
    }
}
