mod policy;

use std::env;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Command, ExitCode};

use boreal_common::os;

use crate::policy::AdminPolicy;

const SYSTEMCTL: &str = "/run/current-system/sw/bin/systemctl";
const UPDATER_LOCK_PATH: &str = "/var/lib/boreal-updater/lock";

unsafe extern "C" {
    fn umask(mask: u32) -> u32;
    fn flock(fd: i32, operation: i32) -> i32;
}

fn sanitize_environment() -> Result<(), String> {
    if os::current_euid() != 0 {
        return Err("boreal-admin-ctl must execute as root".into());
    }
    unsafe {
        umask(0o077);
    }
    env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;
    let keys: Vec<_> = env::vars_os().map(|(k, _)| k).collect();
    for key in keys {
        unsafe {
            env::remove_var(key);
        }
    }
    unsafe {
        env::set_var("PATH", "/run/current-system/sw/bin");
        env::set_var("HOME", "/var/empty");
        env::set_var("LC_ALL", "C");
    }
    Ok(())
}

fn ensure_no_active_updater_lock() -> Result<(), String> {
    let lock_path = Path::new(UPDATER_LOCK_PATH);
    let file = match OpenOptions::new().read(true).write(true).open(lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("inspect updater lock: {e}")),
    };
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
        return Err("cannot modify service state while boreal-updater is actively running".into());
    }
    let _ = unsafe { flock(file.as_raw_fd(), 8) };
    Ok(())
}

fn show_units(units: &[&str]) -> Result<(), String> {
    if units.is_empty() {
        println!("no Boreal Temporal services configured.");
        return Ok(());
    }
    for unit in units {
        let out = Command::new(SYSTEMCTL)
            .args([
                "show",
                "--no-pager",
                "--property=Id,LoadState,ActiveState,SubState,UnitFileState,ExecMainStatus,Result",
                "--",
                unit,
            ])
            .output()
            .map_err(|e| format!("run systemctl show: {e}"))?;
        if !out.status.success() {
            return Err(format!("systemctl show failed for {unit}: {}", out.status));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut id = *unit;
        let mut load = "?";
        let mut active = "?";
        let mut sub = "?";
        let mut result = "?";
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("Id=") {
                id = v;
            } else if let Some(v) = line.strip_prefix("LoadState=") {
                load = v;
            } else if let Some(v) = line.strip_prefix("ActiveState=") {
                active = v;
            } else if let Some(v) = line.strip_prefix("SubState=") {
                sub = v;
            } else if let Some(v) = line.strip_prefix("Result=") {
                result = v;
            }
        }
        println!("{id}  active={active} ({sub})  load={load}  result={result}");
    }
    Ok(())
}

fn validate_hex_rev(rev: &str) -> Result<(), String> {
    if rev.len() != 40 || !rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("revision must be a 40-digit hexadecimal Git commit hash".into());
    }
    Ok(())
}

fn run_ctl(args: &[String], policy: &AdminPolicy) -> Result<(), String> {
    match args {
        [cmd] if cmd == "services" => {
            let refs: Vec<&str> = policy.services.iter().map(String::as_str).collect();
            show_units(&refs)
        }
        [cmd, op, raw_unit] if cmd == "service" => {
            let unit = policy.resolve_service(raw_unit)?;
            match op.as_str() {
                "status" => show_units(&[unit]),
                "start" | "stop" | "restart" => {
                    ensure_no_active_updater_lock()?;
                    let status = Command::new(SYSTEMCTL)
                        .args(["--no-pager", op.as_str(), "--", unit])
                        .status()
                        .map_err(|e| format!("run systemctl {op}: {e}"))?;
                    if !status.success() {
                        return Err(format!("systemctl {op} {unit} failed: {status}"));
                    }
                    show_units(&[unit])
                }
                _ => Err("service operation must be status, start, stop, or restart".into()),
            }
        }
        [cmd, sub, rest @ ..] if cmd == "update" => {
            if !policy.updater_enabled {
                return Err("boreal.updater is not enabled on this host".into());
            }
            let updater = policy
                .updater_bin
                .as_ref()
                .ok_or("missing updater binary in admin policy")?;
            match (sub.as_str(), rest) {
                ("schedule", [rev]) => {
                    validate_hex_rev(rev)?;
                    let st = Command::new(updater)
                        .args(["schedule", rev.as_str()])
                        .status()
                        .map_err(|e| format!("invoke boreal-updater: {e}"))?;
                    if !st.success() {
                        return Err("boreal-updater schedule failed".into());
                    }
                    Ok(())
                }
                ("prebuild", []) => {
                    let st = Command::new(updater)
                        .arg("prebuild")
                        .status()
                        .map_err(|e| format!("invoke boreal-updater: {e}"))?;
                    if !st.success() {
                        return Err("boreal-updater prebuild failed".into());
                    }
                    Ok(())
                }
                ("apply", []) => {
                    let st = Command::new(SYSTEMCTL)
                        .args(["--no-pager", "start", "boreal-updater.service"])
                        .status()
                        .map_err(|e| format!("start boreal-updater.service: {e}"))?;
                    let _ = Command::new(updater).arg("status").status();
                    if !st.success() {
                        return Err("boreal-updater.service did not complete activation".into());
                    }
                    Ok(())
                }
                ("recover", []) => {
                    let st = Command::new(updater)
                        .arg("recover")
                        .status()
                        .map_err(|e| format!("invoke boreal-updater: {e}"))?;
                    if !st.success() {
                        return Err("boreal-updater recover failed".into());
                    }
                    Ok(())
                }
                ("status", []) => {
                    let st = Command::new(updater)
                        .arg("status")
                        .status()
                        .map_err(|e| format!("invoke boreal-updater: {e}"))?;
                    if !st.success() {
                        return Err("boreal-updater status failed".into());
                    }
                    Ok(())
                }
                _ => Err(
                    "usage: update <schedule <40-hex-rev> | prebuild | apply | recover | status>"
                        .into(),
                ),
            }
        }
        [cmd] if cmd == "poweroff" => {
            if !policy.allow_poweroff {
                return Err("poweroff is disabled in this host's admin policy".into());
            }
            let st = Command::new(SYSTEMCTL)
                .args(["--no-pager", "poweroff"])
                .status()
                .map_err(|e| format!("systemctl poweroff: {e}"))?;
            if !st.success() {
                return Err(format!("systemctl poweroff failed: {st}"));
            }
            Ok(())
        }
        _ => Err(
            "usage: boreal-admin-ctl <services | service <status|start|stop|restart> <unit> | update ... | poweroff>"
                .into(),
        ),
    }
}

fn main() -> ExitCode {
    if let Err(err) = sanitize_environment() {
        eprintln!("boreal-admin-ctl: {err}");
        return ExitCode::FAILURE;
    }
    let args: Vec<String> = match env::args_os()
        .skip(1)
        .map(|a| a.into_string().map_err(|_| "non-UTF-8 argument".to_owned()))
        .collect()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("boreal-admin-ctl: {e}");
            return ExitCode::FAILURE;
        }
    };
    let policy = match AdminPolicy::load_system() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("boreal-admin-ctl: {e}");
            return ExitCode::FAILURE;
        }
    };
    match run_ctl(&args, &policy) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("boreal-admin-ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ctl_rejects_unlisted_services_and_bad_revisions() {
        let policy = AdminPolicy::from_yaml(
            "\
services:
- temporal.service
- boreal-worker-alice-greet.service
updaterEnabled: true
updaterBin: /nix/store/x-updater/bin/boreal-updater
allowPoweroff: false
",
        )
        .unwrap();

        for bad in [
            vec!["service", "restart", "sshd"],
            vec!["service", "status", "postgresql"],
            vec!["service", "reload", "temporal"],
            vec!["update", "schedule", "not-a-40-char-sha"],
            vec!["poweroff"],
            vec!["/bin/sh"],
        ] {
            let args: Vec<String> = bad.into_iter().map(str::to_owned).collect();
            assert!(run_ctl(&args, &policy).is_err(), "{args:?}");
        }
    }
}
