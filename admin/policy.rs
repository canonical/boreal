use std::path::{Path, PathBuf};

use boreal_common::os;
use boreal_common::yaml::{self, Value};

pub const DEFAULT_ADMIN_POLICY: &str = "/etc/boreal-admin/policy.yaml";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminPolicy {
    pub services: Vec<String>,
    pub updater_enabled: bool,
    pub updater_bin: Option<PathBuf>,
    pub ctl_bin: PathBuf,
    pub allow_poweroff: bool,
}

impl AdminPolicy {
    pub fn load_system() -> Result<Self, String> {
        Self::load_from_path(Path::new(DEFAULT_ADMIN_POLICY), Some(0))
    }

    pub fn load_from_path(path: &Path, expected_uid: Option<u32>) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            os::verify_dir_owner_and_mode(parent, expected_uid, 0o027)
                .map_err(|e| format!("verify {}: {}", parent.display(), e.detail()))?;
        }
        let bytes = os::read_owned_file(path, expected_uid, 0o227, 65536)
            .map_err(|e| format!("read {}: {}", path.display(), e.detail()))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
        Self::from_yaml(&text)
    }

    pub fn from_yaml(text: &str) -> Result<Self, String> {
        let doc = yaml::parse(text).map_err(|e| format!("parse admin policy: {e:?}"))?;
        let map = doc.as_map().ok_or("admin policy must be a mapping")?;

        let mut services = Vec::new();
        if let Some(seq) = map.get("services").and_then(Value::as_seq) {
            for item in seq {
                let s = item
                    .as_str()
                    .filter(|s| valid_allowed_service(s))
                    .ok_or("invalid unit in admin policy services")?;
                if !services.iter().any(|existing| existing == s) {
                    services.push(s.to_owned());
                }
            }
        }

        let updater_enabled = map
            .get("updaterEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let updater_bin = match map.get("updaterBin") {
            None | Some(Value::Null) => None,
            Some(Value::Str(s)) if s.is_empty() => None,
            Some(Value::Str(s)) => {
                validate_store_exe(s)?;
                Some(PathBuf::from(s))
            }
            _ => return Err("updaterBin must be a string or null".into()),
        };
        if updater_enabled && updater_bin.is_none() {
            return Err("updaterEnabled requires updaterBin store path".into());
        }

        let ctl_str = map
            .get("ctlBin")
            .and_then(Value::as_str)
            .unwrap_or("/run/current-system/sw/bin/boreal-admin-ctl");
        let ctl_bin = PathBuf::from(ctl_str);

        let allow_poweroff = map
            .get("allowPoweroff")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        Ok(Self {
            services,
            updater_enabled,
            updater_bin,
            ctl_bin,
            allow_poweroff,
        })
    }

    pub fn resolve_service(&self, raw: &str) -> Result<&str, String> {
        let candidate = if raw.ends_with(".service") {
            raw.to_owned()
        } else {
            format!("{raw}.service")
        };
        self.services
            .iter()
            .find(|u| **u == candidate)
            .map(String::as_str)
            .ok_or_else(|| format!("service '{raw}' is not permitted for Boreal administration"))
    }
}

fn valid_allowed_service(unit: &str) -> bool {
    if unit == "temporal.service" {
        return true;
    }
    let Some(rest) = unit
        .strip_prefix("boreal-worker-")
        .and_then(|s| s.strip_suffix(".service"))
    else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 100
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

fn validate_store_exe(path: &str) -> Result<(), String> {
    if !path.starts_with("/nix/store/")
        || path.contains("..")
        || path.bytes().any(|b| b.is_ascii_control())
    {
        return Err(format!(
            "executable path '{path}' must reside in /nix/store"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_admin_policy_only_allows_temporal_and_boreal_workers() {
        let yaml = "\
services:
- temporal.service
- boreal-worker-alice-greet.service
updaterEnabled: true
updaterBin: /nix/store/abc-boreal-updater/bin/boreal-updater
ctlBin: /nix/store/def-boreal-admin/bin/boreal-admin-ctl
allowPoweroff: false
";
        let p = AdminPolicy::from_yaml(yaml).unwrap();
        assert_eq!(p.resolve_service("temporal").unwrap(), "temporal.service");
        assert_eq!(
            p.resolve_service("boreal-worker-alice-greet").unwrap(),
            "boreal-worker-alice-greet.service"
        );
        assert!(p.resolve_service("sshd").is_err());
        assert!(p.resolve_service("postgresql").is_err());

        assert!(AdminPolicy::from_yaml("services:\n- sshd.service\n").is_err());
    }
}
