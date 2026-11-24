mod engine;
mod ledger;
mod netns;
mod pg;
mod proxy;
mod remote;
mod temporal;
mod vault;

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boreal_common::catalog::Catalog;
use boreal_common::os;
use boreal_common::wire::{
    self, BROKER_SOCKET, ErrorCode, Request, Response, UPDATER_SOCKET, UpdaterRequest,
    UpdaterResponse,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};

use crate::engine::Engine;
use crate::ledger::{Coordinator, DEFAULT_STATE_DIR};
use crate::pg::DEFAULT_PG_SOCKET;
use crate::proxy::WorkerBinding;
use crate::temporal::{LiveBackend, PostgresConfig};

struct Config {
    etc_dir: PathBuf,
    state_dir: PathBuf,
    broker_socket: PathBuf,
    updater_socket: PathBuf,
    internal_netns: Option<PathBuf>,
    worker_netns_dir: Option<PathBuf>,
    worker_listen_addr: String,
    remote_workers_config: Option<PathBuf>,
    vault_config: Option<PathBuf>,
    upstream_temporal_addr: String,
    pg_socket: PathBuf,
    pg_user: String,
    pg_db: String,
    run_user: Option<String>,
    socket_group: String,
    pg_group: Option<String>,
}

impl Config {
    fn from_env() -> Self {
        Self {
            etc_dir: env::var("BOREAL_ETC_DIR").map_or_else(
                |_| PathBuf::from(boreal_common::catalog::DEFAULT_ETC_DIR),
                PathBuf::from,
            ),
            state_dir: env::var("BOREAL_BROKER_STATE_DIR")
                .map_or_else(|_| PathBuf::from(DEFAULT_STATE_DIR), PathBuf::from),
            broker_socket: env::var("BOREAL_BROKER_SOCKET")
                .map_or_else(|_| PathBuf::from(BROKER_SOCKET), PathBuf::from),
            updater_socket: env::var("BOREAL_UPDATER_SOCKET")
                .map_or_else(|_| PathBuf::from(UPDATER_SOCKET), PathBuf::from),
            internal_netns: env::var("BOREAL_INTERNAL_NETNS").ok().map(PathBuf::from),
            worker_netns_dir: env::var("BOREAL_WORKER_NETNS_DIR").ok().map(PathBuf::from),
            worker_listen_addr: env::var("BOREAL_WORKER_LISTEN_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:7233".into()),
            remote_workers_config: env::var_os("BOREAL_REMOTE_WORKERS_CONFIG").map(PathBuf::from),
            vault_config: env::var_os("BOREAL_VAULT_CONFIG").map(PathBuf::from),
            upstream_temporal_addr: env::var("BOREAL_UPSTREAM_TEMPORAL_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:7233".into()),
            pg_socket: env::var("BOREAL_PG_SOCKET")
                .map_or_else(|_| PathBuf::from(DEFAULT_PG_SOCKET), PathBuf::from),
            pg_user: env::var("BOREAL_PG_USER").unwrap_or_else(|_| "boreal-broker".into()),
            pg_db: env::var("BOREAL_PG_DB").unwrap_or_else(|_| "temporal".into()),
            run_user: env::var("BOREAL_BROKER_USER")
                .ok()
                .filter(|s| !s.is_empty()),
            socket_group: env::var("BOREAL_SOCKET_GROUP").unwrap_or_else(|_| "boreal".into()),
            pg_group: env::var("BOREAL_PG_GROUP").ok().filter(|s| !s.is_empty()),
        }
    }
}

async fn read_async_frame(stream: &mut UnixStream) -> Result<Vec<u8>, String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read frame length: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > wire::MAX_FRAME_BYTES {
        return Err("IPC frame too large".into());
    }
    let mut buf = vec![0u8; len];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| format!("read frame body: {e}"))?;
    Ok(buf)
}

async fn write_async_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<(), String> {
    if payload.len() > wire::MAX_FRAME_BYTES {
        return Err("IPC frame too large".into());
    }
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .map_err(|e| format!("write frame length: {e}"))?;
    stream
        .write_all(payload)
        .await
        .map_err(|e| format!("write frame body: {e}"))?;
    stream.flush().await.map_err(|e| format!("flush: {e}"))
}

async fn serve_user_socket(listener: UnixListener, engine: Arc<Engine<LiveBackend>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            let Ok(std_ref) = stream.into_std() else {
                return;
            };
            let peer_res = os::peer_uid(&std_ref).and_then(os::user_by_uid);
            let Ok(mut stream) = UnixStream::from_std(std_ref) else {
                return;
            };
            let Ok(raw) = read_async_frame(&mut stream).await else {
                return;
            };
            let resp = match peer_res {
                Err(err) => Response::Error {
                    code: ErrorCode::PermissionDenied,
                    message: format!("failed to verify caller OS identity: {}", err.detail()),
                },
                Ok(user) => match Request::decode(&raw) {
                    Ok(req) => engine.handle_user_request(user.name(), req).await,
                    Err(err) => Response::Error {
                        code: ErrorCode::InvalidArgument,
                        message: err.detail(),
                    },
                },
            };
            let _ = write_async_frame(&mut stream, &resp.encode()).await;
        });
    }
}

async fn serve_updater_socket(listener: UnixListener, engine: Arc<Engine<LiveBackend>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            let Ok(std_ref) = stream.into_std() else {
                return;
            };
            let uid_res = os::peer_uid(&std_ref);
            let Ok(mut stream) = UnixStream::from_std(std_ref) else {
                return;
            };
            let Ok(raw) = read_async_frame(&mut stream).await else {
                return;
            };
            let resp = match uid_res {
                Ok(0) => match UpdaterRequest::decode(&raw) {
                    Ok(req) => engine.handle_updater_request(req).await,
                    Err(err) => UpdaterResponse::Error(err.detail()),
                },
                _ => UpdaterResponse::Error("updater socket requires root peer UID".into()),
            };
            let _ = write_async_frame(&mut stream, &resp.encode()).await;
        });
    }
}

struct BoundSockets {
    user_listener: std::os::unix::net::UnixListener,
    updater_listener: std::os::unix::net::UnixListener,
    worker_listeners: Vec<(std::net::TcpListener, WorkerBinding)>,
    remote_listener: Option<(std::net::TcpListener, remote::RemoteIngress)>,
}

fn setup_and_bind(
    cfg: &Config,
    catalog: &Catalog,
    vault: &vault::Policy,
) -> Result<BoundSockets, String> {
    let (broker_uid, broker_gid) = if let Some(user_name) = &cfg.run_user {
        let u = os::user_by_name(user_name).map_err(|e| e.detail())?;
        let gid = os::group_gid_by_name(&cfg.socket_group).map_err(|e| e.detail())?;
        (u.uid(), gid)
    } else {
        (
            os::current_euid(),
            os::user_by_uid(os::current_euid())
                .map_err(|e| e.detail())?
                .gid(),
        )
    };

    let remote_listener = match &cfg.remote_workers_config {
        Some(path) => {
            if os::current_euid() != 0 || broker_uid == 0 {
                return Err(
                    "remote worker ingress requires root startup and a non-root broker user".into(),
                );
            }
            // This listener belongs to the host netns; load its credentials
            // before joining the internal netns and dropping root.
            Some(remote::RemoteIngress::bind(path, catalog, 0)?)
        }
        None => None,
    };

    let user_listener = netns::bind_unix_socket(&cfg.broker_socket, broker_uid, broker_gid, 0o660)?;
    let updater_listener = netns::bind_unix_socket(&cfg.updater_socket, 0, 0, 0o600)?;

    let mut worker_listeners = Vec::new();
    if cfg.worker_netns_dir.is_none() && vault.owners().next().is_some() {
        return Err("vaults require isolated worker network namespaces".into());
    }
    if let Some(netns_dir) = &cfg.worker_netns_dir {
        for (owner, policy) in catalog.users() {
            for worker_name in policy.workers().keys() {
                let unit_key = format!("{owner}-{worker_name}");
                let ns_path = netns_dir.join(format!("boreal-worker-{unit_key}"));
                let tcp = netns::bind_tcp_in_netns(&ns_path, &cfg.worker_listen_addr)?;
                worker_listeners.push((
                    tcp,
                    WorkerBinding {
                        owner: owner.clone(),
                        task_queue: unit_key,
                    },
                ));
            }
        }
        for owner in vault.owners() {
            let queue = format!("{owner}-vault");
            let ns_path = netns_dir.join(format!("boreal-worker-{queue}"));
            let tcp = netns::bind_tcp_in_netns(&ns_path, &cfg.worker_listen_addr)?;
            worker_listeners.push((
                tcp,
                WorkerBinding {
                    owner: owner.to_owned(),
                    task_queue: queue,
                },
            ));
        }
    }

    if let Some(internal_ns) = &cfg.internal_netns {
        netns::enter_netns(internal_ns)?;
    }

    if let Some(user_name) = &cfg.run_user {
        if !cfg.state_dir.exists() {
            std::fs::create_dir_all(&cfg.state_dir)
                .map_err(|e| format!("create {}: {e}", cfg.state_dir.display()))?;
        }
        let u = os::user_by_name(user_name).map_err(|e| e.detail())?;
        chown_recursive(&cfg.state_dir, u.uid(), u.gid())?;
        let mut groups = Vec::new();
        if let Some(pg_grp) = &cfg.pg_group {
            groups.push(pg_grp.as_str());
        }
        netns::drop_privileges(user_name, &groups)?;
    }

    Ok(BoundSockets {
        user_listener,
        updater_listener,
        worker_listeners,
        remote_listener,
    })
}

fn chown_recursive(dir: &Path, uid: u32, gid: u32) -> Result<(), String> {
    unsafe extern "C" {
        fn chown(path: *const std::ffi::c_char, owner: u32, group: u32) -> i32;
    }
    let chown_one = |p: &Path| -> Result<(), String> {
        let c = std::ffi::CString::new(p.to_str().ok_or("non-UTF-8 path")?)
            .map_err(|_| "NUL in path")?;
        if unsafe { chown(c.as_ptr(), uid, gid) } != 0 {
            return Err(format!(
                "chown {}: {}",
                p.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    };
    chown_one(dir)?;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        chown_one(&entry.path())?;
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args_os().nth(1).is_some() {
        return Err("usage: boreal-broker".into());
    }

    let cfg = Config::from_env();
    let expected_owner = if os::current_euid() == 0 {
        Some(0)
    } else {
        None
    };
    let catalog = Catalog::load_from_dir(&cfg.etc_dir, expected_owner)
        .map_err(|e| format!("load catalog {}: {}", cfg.etc_dir.display(), e.detail()))?;
    let vault = Arc::new(match cfg.vault_config.as_deref() {
        Some(path) => vault::Policy::load(path, &catalog)?,
        None => vault::Policy::default(),
    });

    // Bind sockets, enter boreal-internal netns, and drop privileges while single-threaded
    // before spawning any Tokio runtime threads.
    let bound = setup_and_bind(&cfg, &catalog, &vault)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_broker(cfg, catalog, bound, vault))
}

async fn run_broker(
    cfg: Config,
    catalog: Catalog,
    bound: BoundSockets,
    vault: Arc<vault::Policy>,
) -> Result<(), Box<dyn std::error::Error>> {
    let coordinator = Arc::new(
        Coordinator::open(&cfg.state_dir, bound.remote_listener.is_some())
            .map_err(|e| format!("open state {}: {}", cfg.state_dir.display(), e.detail()))?,
    );
    let backend = Arc::new(LiveBackend::new(
        &cfg.upstream_temporal_addr,
        PostgresConfig {
            socket: cfg.pg_socket,
            user: cfg.pg_user,
            database: cfg.pg_db,
        },
    ));
    let catalog = Arc::new(catalog);
    let engine = Arc::new(Engine::new(
        catalog.as_ref().clone(),
        Arc::clone(&coordinator),
        backend,
    ));

    let user_listener = UnixListener::from_std(bound.user_listener)?;
    let updater_listener = UnixListener::from_std(bound.updater_listener)?;

    if let Some((std_tcp, ingress)) = bound.remote_listener {
        let tcp = TcpListener::from_std(std_tcp)?;
        tokio::spawn(proxy::serve_remote_listener(
            tcp,
            ingress,
            cfg.upstream_temporal_addr.clone(),
            Arc::clone(&coordinator),
            Arc::clone(&catalog),
            Arc::clone(&vault),
        ));
    }

    for (std_tcp, binding) in bound.worker_listeners {
        let tcp = TcpListener::from_std(std_tcp)?;
        tokio::spawn(proxy::serve_worker_listener(
            tcp,
            binding,
            cfg.upstream_temporal_addr.clone(),
            Arc::clone(&coordinator),
            Arc::clone(&catalog),
            Arc::clone(&vault),
        ));
    }

    tokio::spawn(serve_user_socket(user_listener, Arc::clone(&engine)));
    tokio::spawn(serve_updater_socket(updater_listener, Arc::clone(&engine)));

    wait_for_shutdown().await?;
    Ok(())
}

async fn wait_for_shutdown() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}
