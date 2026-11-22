//! Publish and pin non-confidential files through a Nix binary cache.

use std::{
    env, fmt,
    fs::{self, DirBuilder},
    io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

const NAME: &str = "boreal-artifact";
const NIX32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref(String);

impl Ref {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Ref {
    type Err = Error;

    fn from_str(path: &str) -> Result<Self, Error> {
        let Some((hash, name)) = path
            .strip_prefix("/nix/store/")
            .and_then(|s| s.split_once('-'))
        else {
            return Err(Error::InvalidRef);
        };
        if name != NAME || hash.len() != 32 || !hash.bytes().all(|b| NIX32.contains(&b)) {
            return Err(Error::InvalidRef);
        }
        Ok(Self(path.to_owned()))
    }
}

#[derive(Debug)]
pub enum Error {
    MissingConfig(&'static str),
    PublishDisabled,
    NotRegularFile,
    InvalidRef,
    InvalidRoot,
    Io(io::Error),
    Nix {
        operation: &'static str,
        status: ExitStatus,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingConfig(name) => write!(f, "missing artifact configuration: {name}"),
            Self::PublishDisabled => f.write_str("artifact publishing is not configured"),
            Self::NotRegularFile => f.write_str("artifact input must be a regular file"),
            Self::InvalidRef => f.write_str("invalid Boreal artifact reference"),
            Self::InvalidRoot => f.write_str("Nix did not pin the requested artifact"),
            Self::Io(err) => write!(f, "artifact I/O failed: {err}"),
            Self::Nix { operation, status } => {
                write!(f, "Nix artifact {operation} failed ({status})")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

pub struct Store {
    read: String,
    write: Option<String>,
    runtime: PathBuf,
}

impl Store {
    /// Load options supplied to the worker service by the Boreal NixOS module.
    pub fn from_env() -> Result<Self, Error> {
        const READ: &str = "BOREAL_ARTIFACT_READ_STORE";
        const RUNTIME: &str = "BOREAL_ARTIFACT_RUNTIME_DIR";
        let read = env::var(READ)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or(Error::MissingConfig(READ))?;
        let runtime = env::var_os(RUNTIME)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(Error::MissingConfig(RUNTIME))?;
        let write = env::var("BOREAL_ARTIFACT_WRITE_STORE")
            .ok()
            .filter(|s| !s.is_empty());
        Ok(Self {
            read,
            write,
            runtime,
        })
    }

    /// Import a closed file and return its reference only after Nix has uploaded it.
    /// This blocks; call it from an Activity, not from Workflow code.
    pub fn publish_file(&self, path: impl AsRef<Path>) -> Result<Ref, Error> {
        let write = self.write.as_deref().ok_or(Error::PublishDisabled)?;
        let path = path.as_ref();
        if !path.symlink_metadata()?.is_file() {
            return Err(Error::NotRegularFile);
        }
        let output = run(
            self.nix()
                .args(["store", "add", "--mode", "flat", "--name", NAME, "--"])
                .arg(path),
            "import",
        )?;
        let reference: Ref = std::str::from_utf8(&output.stdout)
            .map_err(|_| Error::InvalidRef)?
            .trim_end_matches('\n')
            .parse()?;
        // Keep the import alive during the transfer. If collection won the race
        // between import and pin, the Activity can retry the whole publish.
        let _pin = self.pin_local(&reference)?;
        run(
            self.nix()
                .args(["copy", "--refresh", "--to", write, reference.as_str()]),
            "publish",
        )?;
        Ok(reference)
    }

    /// Pin the host copy for the life of the returned handle. Fetch from central
    /// only if the local Nix store cannot supply the path. This blocks.
    pub fn fetch(&self, reference: &Ref) -> Result<Pinned, Error> {
        match self.pin_local(reference) {
            Err(Error::Nix { .. }) => {
                let root = Root::create(&self.runtime)?;
                run(
                    self.nix()
                        .args(["copy", "--refresh", "--from", &self.read, "--out-link"])
                        .arg(&root.link)
                        .arg(reference.as_str()),
                    "fetch",
                )?;
                root.pin(reference)
            }
            result => result,
        }
    }

    fn pin_local(&self, reference: &Ref) -> Result<Pinned, Error> {
        let root = Root::create(&self.runtime)?;
        run(
            self.command("nix-store")
                .args(["--realise", "--option", "substitute", "false", "--add-root"])
                .arg(&root.link)
                .arg(reference.as_str()),
            "local pin",
        )?;
        root.pin(reference)
    }

    fn nix(&self) -> Command {
        let mut command = self.command("nix");
        command.args(["--extra-experimental-features", "nix-command"]);
        command
    }

    fn command(&self, bin: &str) -> Command {
        let mut command = Command::new(bin);
        command.env("XDG_CACHE_HOME", self.runtime.join("metadata"));
        command
    }
}

fn run(command: &mut Command, operation: &'static str) -> Result<Output, Error> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output)
    } else {
        // External diagnostics may include private paths or server responses.
        Err(Error::Nix {
            operation,
            status: output.status,
        })
    }
}

pub struct Pinned {
    reference: Ref,
    _root: Root,
}

impl Pinned {
    /// Read-only path shared by workers on the same host. Keep this handle alive
    /// until the last reader (including a subprocess) has finished.
    pub fn path(&self) -> &Path {
        Path::new(self.reference.as_str())
    }
}

struct Root {
    dir: PathBuf,
    link: PathBuf,
}

impl Root {
    fn create(runtime: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = runtime.join(format!("pin-{}-{id}", std::process::id()));
            match DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    return Ok(Self {
                        link: dir.join("artifact"),
                        dir,
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
    }

    fn pin(self, reference: &Ref) -> Result<Pinned, Error> {
        if self.link.read_link()? != Path::new(reference.as_str()) {
            return Err(Error::InvalidRoot);
        }
        Ok(Pinned {
            reference: reference.clone(),
            _root: self,
        })
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.link);
        let _ = fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ref_rejects_non_artifact_paths() {
        let valid = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-boreal-artifact";
        assert_eq!(valid.parse::<Ref>().unwrap().as_str(), valid);
        for invalid in [
            valid.replace("artifact", "artifact.drv"),
            valid.replace('0', "e"),
            valid.replace('0', ""),
            format!("{valid}/file"),
            format!("{valid}\n"),
            format!("/{valid}"),
        ] {
            assert!(matches!(invalid.parse::<Ref>(), Err(Error::InvalidRef)));
        }
    }
}
