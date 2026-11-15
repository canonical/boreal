use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use boreal_common::os;
use boreal_common::yaml::{self, Value};

pub const FILE_NAME: &str = ".borealrc";
pub const MAX_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Emacs,
    Vi,
    Helix,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Config {
    edit_mode: Mode,
}

impl Config {
    pub fn edit_mode(&self) -> Mode {
        self.edit_mode
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    UnsafeFile(String),
    Io(String),
    InvalidYaml(String),
    UnknownKey(String),
    InvalidEditMode(String),
}

impl Error {
    pub fn detail(&self) -> String {
        match self {
            Self::UnsafeFile(msg) => msg.clone(),
            Self::Io(msg) => msg.clone(),
            Self::InvalidYaml(msg) => msg.clone(),
            Self::UnknownKey(key) => format!("unknown key '{key}'"),
            Self::InvalidEditMode(val) => {
                format!("edit_mode must be 'emacs', 'vi', or 'helix' (got '{val}')")
            }
        }
    }
}

pub fn parse_mode<S: AsRef<str>>(raw: S) -> Result<Mode, Error> {
    match raw.as_ref() {
        "emacs" => Ok(Mode::Emacs),
        "vi" => Ok(Mode::Vi),
        "helix" => Ok(Mode::Helix),
        other => Err(Error::InvalidEditMode(other.to_owned())),
    }
}

pub fn from_yaml<S: AsRef<str>>(raw: S) -> Result<Config, Error> {
    let parsed = yaml::parse(raw.as_ref()).map_err(|err| match err {
        yaml::Error::Syntax { line, detail } => {
            Error::InvalidYaml(format!("line {line}: {detail}"))
        }
        yaml::Error::DuplicateKey { line, key } => {
            Error::InvalidYaml(format!("line {line}: duplicate key '{key}'"))
        }
    })?;

    match parsed {
        Value::Null => Ok(Config::default()),
        Value::Map(map) => {
            let mut cfg = Config::default();
            for (key, val) in map {
                match key.as_str() {
                    "edit_mode" => {
                        let s = val
                            .as_str()
                            .ok_or_else(|| Error::InvalidEditMode("non-string value".into()))?;
                        cfg.edit_mode = parse_mode(s)?;
                    }
                    _ => return Err(Error::UnknownKey(key)),
                }
            }
            Ok(cfg)
        }
        _ => Err(Error::InvalidYaml(
            "root value must be a YAML mapping".into(),
        )),
    }
}

pub fn load_from_home<P: AsRef<Path>>(home: P, owner_uid: u32) -> Result<Config, Error> {
    let path = home.as_ref().join(FILE_NAME);
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() || meta.uid() != owner_uid {
                return Err(Error::UnsafeFile(format!(
                    "{} must be a regular file owned by uid {owner_uid}",
                    path.display()
                )));
            }
            if meta.mode() & 0o022 != 0 {
                return Err(Error::UnsafeFile(format!(
                    "{} must not be group or world writable",
                    path.display()
                )));
            }
            if meta.len() > MAX_BYTES as u64 {
                return Err(Error::UnsafeFile(format!(
                    "{} exceeds maximum size ({MAX_BYTES} bytes)",
                    path.display()
                )));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => return Err(Error::Io(err.to_string())),
    }

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(os::O_NOFOLLOW)
        .open(&path)
        .map_err(|err| Error::Io(err.to_string()))?;
    let meta = file.metadata().map_err(|err| Error::Io(err.to_string()))?;
    if !meta.is_file() || meta.uid() != owner_uid || meta.mode() & 0o022 != 0 {
        return Err(Error::UnsafeFile(format!(
            "{} failed ownership or permission verification",
            path.display()
        )));
    }

    let mut raw = String::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_string(&mut raw)
        .map_err(|err| Error::Io(err.to_string()))?;
    if raw.len() > MAX_BYTES {
        return Err(Error::UnsafeFile(format!(
            "{} exceeds maximum size ({MAX_BYTES} bytes)",
            path.display()
        )));
    }
    from_yaml(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_from_yaml_parses_modes_and_rejects_invalid_inputs() {
        let valid_cases = [
            ("", Mode::Emacs),
            ("# comment only\n", Mode::Emacs),
            ("edit_mode: emacs\n", Mode::Emacs),
            ("edit_mode: vi\n", Mode::Vi),
            ("edit_mode: helix\n", Mode::Helix),
        ];
        for (input, expected) in valid_cases {
            assert_eq!(from_yaml(input).unwrap().edit_mode(), expected, "{input:?}");
        }

        assert!(matches!(
            from_yaml("edit_mode: nano\n"),
            Err(Error::InvalidEditMode(_))
        ));
        assert!(matches!(
            from_yaml("edit_mode: 42\n"),
            Err(Error::InvalidEditMode(_))
        ));
        assert!(matches!(
            from_yaml("unknown_key: true\n"),
            Err(Error::UnknownKey(_))
        ));
        assert!(matches!(
            from_yaml("- list_item\n"),
            Err(Error::InvalidYaml(_))
        ));
    }

    #[test]
    fn test_load_from_home_enforces_nofollow_and_permissions() {
        struct TempDir(std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let uid = os::current_euid();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let guard = TempDir(
            Path::new("/tmp/opencode")
                .join(format!("borealrc-test-{}-{stamp}", std::process::id())),
        );
        let dir = &guard.0;
        fs::create_dir_all(dir).unwrap();

        // Missing ~/.borealrc defaults cleanly to Emacs.
        assert_eq!(load_from_home(dir, uid).unwrap().edit_mode(), Mode::Emacs);

        let rc_path = dir.join(FILE_NAME);
        fs::write(&rc_path, "edit_mode: helix\n").unwrap();
        fs::set_permissions(&rc_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load_from_home(dir, uid).unwrap().edit_mode(), Mode::Helix);

        // Group/world-writable ~/.borealrc is rejected.
        fs::set_permissions(&rc_path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            load_from_home(dir, uid),
            Err(Error::UnsafeFile(_))
        ));

        // Symlinked ~/.borealrc is rejected via symlink check + O_NOFOLLOW.
        fs::remove_file(&rc_path).unwrap();
        let target = dir.join("other.yaml");
        fs::write(&target, "edit_mode: vi\n").unwrap();
        symlink(&target, &rc_path).unwrap();
        assert!(matches!(
            load_from_home(dir, uid),
            Err(Error::UnsafeFile(_))
        ));
    }
}
