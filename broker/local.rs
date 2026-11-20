use std::ffi::OsString;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boreal_common::{catalog::Catalog, os};
use tokio::net::UnixListener;

use crate::engine::Engine;
use crate::ledger::Coordinator;
use crate::temporal::LiveBackend;
use crate::{UserIdentity, serve_user_socket, wait_for_shutdown};

pub(crate) const PRINCIPAL: &str = "local";
const TEMPORAL_ADDR: &str = "127.0.0.1:7233";
const USAGE: &str = "usage: boreal-broker local <catalog-dir> <broker-socket> <state-dir>";

struct Config {
    catalog_dir: PathBuf,
    broker_socket: PathBuf,
    state_dir: PathBuf,
}

impl Config {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Self, &'static str> {
        match (args.next(), args.next(), args.next(), args.next()) {
            (Some(catalog), Some(socket), Some(state), None)
                if !catalog.is_empty() && !socket.is_empty() && !state.is_empty() =>
            {
                Ok(Self {
                    catalog_dir: catalog.into(),
                    broker_socket: socket.into(),
                    state_dir: state.into(),
                })
            }
            _ => Err(USAGE),
        }
    }
}

pub(crate) fn run(args: impl Iterator<Item = OsString>) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::parse(args)?;
    let uid = os::current_euid();
    if uid == 0 {
        return Err("local broker must run as a non-root user".into());
    }

    let catalog = load_catalog(&cfg.catalog_dir)?;
    let coordinator = Coordinator::open(&cfg.state_dir, false)
        .map_err(|e| format!("open state {}: {}", cfg.state_dir.display(), e.detail()))?;
    let engine = Arc::new(Engine::new(
        catalog,
        Arc::new(coordinator),
        Arc::new(LiveBackend::new(TEMPORAL_ADDR, None)),
    ));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = bind_socket(&cfg.broker_socket, uid)?;
        tokio::select! {
            _ = serve_user_socket(listener, engine, UserIdentity::Local { uid }) => {}
            result = wait_for_shutdown() => result?,
        }
        Ok(())
    })
}

fn load_catalog(dir: &Path) -> Result<Catalog, String> {
    // The launcher supplies a trusted catalog; schema exports may be store-owned.
    let catalog = Catalog::load_from_dir(dir, None)
        .map_err(|e| format!("load catalog {}: {}", dir.display(), e.detail()))?;
    if catalog.users().len() != 1 || !catalog.is_user(PRINCIPAL) {
        return Err("local catalog must declare exactly one user named 'local'".into());
    }
    Ok(catalog)
}

fn bind_socket(path: &Path, uid: u32) -> Result<UnixListener, String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    os::verify_dir_owner_and_mode(parent, Some(uid), 0o077).map_err(|e| {
        format!(
            "verify local socket directory {}: {}",
            parent.display(),
            e.detail()
        )
    })?;
    // The launcher serializes startup and owns stale-socket cleanup. Bind
    // must fail if anything already exists at the requested path.
    let listener = UnixListener::bind(path)
        .map_err(|e| format!("bind local socket {}: {e}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|e| format!("set permissions on local socket {}: {e}", path.display()))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, OpenOptionsExt};

    use boreal_common::wire::{ErrorCode, Request, Response};
    use tokio::net::UnixStream;

    use super::*;
    use crate::{read_async_frame, write_async_frame};

    #[test]
    fn local_arguments_require_exactly_three_paths() {
        let parse = |args: &[&str]| Config::parse(args.iter().map(OsString::from));
        let cfg = parse(&["/catalog", "/ipc/broker.sock", "/state"]).unwrap();
        assert_eq!(cfg.catalog_dir, Path::new("/catalog"));
        assert_eq!(cfg.broker_socket, Path::new("/ipc/broker.sock"));
        assert_eq!(cfg.state_dir, Path::new("/state"));
        assert!(parse(&["/catalog", "/ipc/broker.sock"]).is_err());
        assert!(parse(&["/catalog", "", "/state"]).is_err());
        assert!(parse(&["/catalog", "/ipc/broker.sock", "/state", "extra"]).is_err());
    }

    fn catalog_dir(users: &[&str]) -> tempfile::TempDir {
        use std::io::Write;

        let write_private = |path, text: &str| {
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o400)
                .open(path)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        };
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        let dir = private_tempdir();
        write_private(dir.path().join("schemas.yaml"), "{}");
        builder.create(dir.path().join("users")).unwrap();
        for user in users {
            let user_dir = dir.path().join("users").join(user);
            builder.create(&user_dir).unwrap();
            write_private(
                user_dir.join("perms.yaml"),
                &format!("user: {user}\npermissions:\n  read: false\n  write: false\n"),
            );
        }
        dir
    }

    fn private_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn local_catalog_rejects_missing_or_additional_principals() {
        for users in [&[][..], &["alice"], &[PRINCIPAL, "alice"]] {
            let dir = catalog_dir(users);
            let err = load_catalog(dir.path()).unwrap_err();
            assert!(err.contains("exactly one user named 'local'"), "{err}");
        }
        assert!(load_catalog(catalog_dir(&[PRINCIPAL]).path()).is_ok());
    }

    #[tokio::test]
    async fn local_bind_never_replaces_an_existing_path() {
        let dir = private_tempdir();
        let socket = dir.path().join("broker.sock");
        let uid = os::current_euid();

        fs::write(&socket, b"existing file").unwrap();
        assert!(bind_socket(&socket, uid).is_err());
        assert_eq!(fs::read(&socket).unwrap(), b"existing file");
        fs::remove_file(&socket).unwrap();

        drop(bind_socket(&socket, uid).unwrap());
        assert!(bind_socket(&socket, uid).is_err());
        let metadata = fs::symlink_metadata(&socket).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn local_bind_requires_an_owned_private_parent() {
        let dir = private_tempdir();
        let socket = dir.path().join("broker.sock");
        let uid = os::current_euid();
        assert!(bind_socket(&socket, uid ^ 1).is_err());
        fs::set_permissions(dir.path(), Permissions::from_mode(0o750)).unwrap();
        assert!(bind_socket(&socket, uid).is_err());
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn local_socket_maps_only_the_configured_peer_uid_to_local() {
        let dir = catalog_dir(&[PRINCIPAL]);
        let catalog = load_catalog(dir.path()).unwrap();
        let coordinator = Coordinator::open(&dir.path().join("state"), false).unwrap();
        let engine = Arc::new(Engine::new(
            catalog,
            Arc::new(coordinator),
            Arc::new(LiveBackend::new(TEMPORAL_ADDR, None)),
        ));
        let uid = os::current_euid();

        for (index, expected_uid) in [uid, uid ^ 1].into_iter().enumerate() {
            let socket = dir.path().join(format!("broker-{index}.sock"));
            let listener = bind_socket(&socket, uid).unwrap();
            let server = tokio::spawn(serve_user_socket(
                listener,
                Arc::clone(&engine),
                UserIdentity::Local { uid: expected_uid },
            ));
            let mut client = UnixStream::connect(&socket).await.unwrap();
            let req = Request::Permissions { user: None };
            write_async_frame(&mut client, &req.encode()).await.unwrap();
            let frame = read_async_frame(&mut client).await.unwrap();
            match Response::decode(&frame).unwrap() {
                Response::Permissions(report) if uid == expected_uid => {
                    assert_eq!(report.user, PRINCIPAL);
                    assert!(report.is_self);
                }
                Response::Error {
                    code: ErrorCode::PermissionDenied,
                    ..
                } if uid != expected_uid => {}
                response => panic!("unexpected response: {response:?}"),
            }
            server.abort();
        }
    }
}
