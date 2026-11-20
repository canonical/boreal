use std::ffi::OsStr;
use std::os::unix::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use boreal_common::{catalog::Catalog, os};

use crate::{client::Mode, command::Context};

const LOCAL_CALLER: &str = "local";

pub struct LocalProfile {
    home: PathBuf,
    context: Arc<Context>,
    socket: PathBuf,
}

impl LocalProfile {
    pub fn open(home: &OsStr, catalog_dir: &OsStr, socket: &OsStr) -> Result<Self, String> {
        let home = absolute_path(home, "profile-home")?;
        let catalog_dir = absolute_path(catalog_dir, "catalog-dir")?;
        let socket = absolute_path(socket, "broker-socket")?;
        SocketAddr::from_pathname(&socket)
            .map_err(|e| format!("invalid local broker socket: {e}"))?;
        os::verify_dir_owner_and_mode(&home, Some(os::current_euid()), 0o077)
            .map_err(|e| format!("invalid local profile home: {}", e.detail()))?;

        // Local catalogs can reference immutable, root-owned Nix schemas.
        let catalog = Catalog::load_from_dir(&catalog_dir, None)
            .map_err(|e| format!("invalid local catalog: {}", e.detail()))?;
        if catalog.users().len() != 1 || !catalog.is_user(LOCAL_CALLER) {
            return Err("local catalog must contain exactly the 'local' account".into());
        }

        Ok(Self {
            home,
            context: Arc::new(Context::new(catalog, LOCAL_CALLER)),
            socket,
        })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn runtime(&self) -> (Arc<Context>, Mode) {
        (
            Arc::clone(&self.context),
            Mode::Live {
                socket: self.socket.clone(),
            },
        )
    }
}

fn absolute_path(arg: &OsStr, name: &str) -> Result<PathBuf, String> {
    let path = Path::new(arg);
    if !path.is_absolute() {
        return Err(format!("--local requires an absolute {name} path"));
    }
    // Remove trailing '/' or '/.' so they cannot mask a final symlink in the
    // profile home or catalog directory ownership checks.
    Ok(path.components().collect())
}
