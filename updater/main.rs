use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_DIR: &str = "/var/lib/boreal-updater";
const BLOCKED: &str =
    "activation blocked: no enforced admission gate and authoritative drain protocol";

unsafe extern "C" {
    fn geteuid() -> u32;
    fn flock(fd: i32, operation: i32) -> i32;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    repo: String,
    rev: String,
    host: String,
}

impl Target {
    fn new(repo: &str, rev: &str, host: &str) -> Result<Self, String> {
        // Restrict the syntax rather than accepting query parameters, credentials,
        // URL escapes, or strings that could change the flake reference or expression.
        let rest = repo
            .strip_prefix("https://")
            .ok_or("repository must use https://")?;
        let (domain, path) = rest
            .split_once('/')
            .ok_or("repository URL must include a path")?;
        if domain.is_empty()
            || !domain
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
            || path.is_empty()
            || !path
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/._~-".contains(&c))
        {
            return Err("repository URL must contain only a host and an unescaped path (no credentials, query, or fragment)".into());
        }
        if rev.len() != 40 || !rev.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("revision must be a full 40-digit Git commit hash".into());
        }
        if host.is_empty()
            || host.len() > 63
            || !host.as_bytes()[0].is_ascii_alphanumeric()
            || !host.as_bytes()[host.len() - 1].is_ascii_alphanumeric()
            || !host.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err("host must be a single DNS-style hostname".into());
        }
        Ok(Self {
            repo: repo.into(),
            rev: rev.to_ascii_lowercase(),
            host: host.into(),
        })
    }

    fn flake(&self) -> String {
        format!("git+{}?rev={}", self.repo, self.rev)
    }

    fn installable(&self) -> String {
        format!(
            "{}#nixosConfigurations.{}.config.system.build.toplevel",
            self.flake(),
            self.host
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Scheduled,
    Building,
    Failed,
    Blocked,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Building => "building",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug)]
struct State {
    target: Target,
    phase: Phase,
    result: Option<PathBuf>,
}

impl State {
    fn encode(&self) -> String {
        format!(
            "version=1\nphase={}\nrepo={}\nrev={}\nhost={}\nresult={}\n",
            self.phase.name(),
            self.target.repo,
            self.target.rev,
            self.target.host,
            self.result
                .as_deref()
                .map_or("", |p| p.to_str().unwrap_or(""))
        )
    }

    fn decode(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        fn field<'a>(
            lines: &mut impl Iterator<Item = &'a str>,
            name: &str,
        ) -> Result<&'a str, String> {
            lines
                .next()
                .and_then(|line| line.strip_prefix(name))
                .ok_or_else(|| format!("invalid state: missing {name}"))
        }
        if field(&mut lines, "version=")? != "1" {
            return Err("unsupported state version".into());
        }
        let phase = match field(&mut lines, "phase=")? {
            "scheduled" => Phase::Scheduled,
            "building" => Phase::Building,
            "failed" => Phase::Failed,
            "blocked" => Phase::Blocked,
            _ => return Err("invalid state phase".into()),
        };
        let repo = field(&mut lines, "repo=")?;
        let rev = field(&mut lines, "rev=")?;
        let host = field(&mut lines, "host=")?;
        let result = field(&mut lines, "result=")?;
        if lines.next().is_some() {
            return Err("unexpected state fields".into());
        }
        let result = if result.is_empty() {
            None
        } else {
            Some(store_path(result)?)
        };
        if (phase == Phase::Blocked) != result.is_some() {
            return Err("invalid phase/result combination".into());
        }
        Ok(Self {
            target: Target::new(repo, rev, host)?,
            phase,
            result,
        })
    }
}

fn store_path(s: &str) -> Result<PathBuf, String> {
    let tail = s
        .strip_prefix("/nix/store/")
        .ok_or("build result is not a Nix store path")?;
    if tail.is_empty() || tail.contains('/') || tail.bytes().any(|c| c.is_ascii_control()) {
        return Err("invalid Nix store path".into());
    }
    Ok(PathBuf::from(s))
}

fn prepare_dir(dir: &Path) -> Result<(), String> {
    // The parent must not permit unprivileged replacement of the state directory.
    let parent = dir.parent().ok_or("state directory has no parent")?;
    let meta =
        fs::symlink_metadata(parent).map_err(|e| format!("inspect {}: {e}", parent.display()))?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} must be a root-owned directory not writable by group or others",
            parent.display()
        ));
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(format!("create {}: {e}", dir.display())),
    }
    let meta = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be a root-owned directory inaccessible to group and others",
            dir.display()
        ));
    }
    Ok(())
}

// Holding this lock across the entire build ensures that another schedule or
// worker cannot replace its target. Status reads the atomically replaced file.
fn lock(dir: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("lock"))
        .map_err(|e| format!("open lock: {e}"))?;
    // LOCK_EX | LOCK_NB; a second scheduler or worker fails rather than waiting.
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
        return Err(format!(
            "another updater operation may be running: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(file)
}

fn load(dir: &Path) -> Result<State, String> {
    let text = fs::read_to_string(dir.join("state")).map_err(|e| format!("read state: {e}"))?;
    State::decode(&text)
}

fn save(dir: &Path, state: &State) -> Result<(), String> {
    let tmp = dir.join("state.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("open temporary state: {e}"))?;
    file.write_all(state.encode().as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("write state: {e}"))?;
    fs::rename(&tmp, dir.join("state")).map_err(|e| format!("replace state: {e}"))?;
    File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("sync state directory: {e}"))
}

trait Builder {
    fn build(&self, target: &Target, result_link: &Path) -> Result<PathBuf, String>;
}

struct NixBuilder;

impl NixBuilder {
    fn source_command(target: &Target) -> Command {
        // Resolve the exact source first and demand an actual, committed lock file.
        // Target validation excludes expression metacharacters.
        let expr = format!("(builtins.getFlake \"{}\").outPath", target.flake());
        let mut command = Command::new("nix");
        command.args([
            "--extra-experimental-features",
            "nix-command flakes",
            "eval",
            "--no-update-lock-file",
            "--no-write-lock-file",
            "--raw",
            "--expr",
            &expr,
        ]);
        command
    }

    fn build_command(target: &Target, result_link: &Path) -> Command {
        let mut command = Command::new("nix");
        command
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "build",
                "--no-update-lock-file",
                "--no-write-lock-file",
                "--out-link",
            ])
            .arg(result_link)
            .arg(target.installable());
        command
    }
}

impl Builder for NixBuilder {
    fn build(&self, target: &Target, result_link: &Path) -> Result<PathBuf, String> {
        let output = Self::source_command(target)
            .output()
            .map_err(|e| format!("start nix eval: {e}"))?;
        if !output.status.success() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            return Err(format!("nix source resolution failed: {}", output.status));
        }
        let source =
            String::from_utf8(output.stdout).map_err(|_| "nix returned a non-UTF-8 source path")?;
        let source = store_path(source.trim())?;
        let lock_meta = fs::symlink_metadata(source.join("flake.lock"))
            .map_err(|e| format!("pinned source must contain flake.lock: {e}"))?;
        if !lock_meta.is_file() {
            return Err("pinned flake.lock must be a regular file".into());
        }
        let status = Self::build_command(target, result_link)
            .status()
            .map_err(|e| format!("start nix build: {e}"))?;
        if !status.success() {
            return Err(format!("nix build failed: {status}"));
        }
        let path =
            fs::read_link(result_link).map_err(|e| format!("read build result link: {e}"))?;
        store_path(path.to_str().ok_or("non-UTF-8 build result")?)
    }
}

// Every attempt has a distinct result link. If the worker is killed while a Nix
// child survives, that child cannot replace the result of a later schedule.
fn attempt_dir(dir: &Path) -> Result<PathBuf, String> {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    for sequence in 0..1000 {
        let path = dir.join(format!("attempt-{}-{time}-{sequence}", std::process::id()));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                File::open(dir)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| format!("sync attempt directory: {e}"))?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create attempt directory: {e}")),
        }
    }
    Err("could not allocate a unique attempt directory".into())
}

fn run_worker(dir: &Path, builder: &impl Builder) -> Result<(), String> {
    let _lock = lock(dir)?;
    let mut state = load(dir)?;
    if state.phase == Phase::Blocked {
        return Err(BLOCKED.into());
    }
    state.phase = Phase::Building;
    state.result = None;
    save(dir, &state)?;
    let result = attempt_dir(dir)
        .and_then(|attempt| builder.build(&state.target, &attempt.join("result")))
        .and_then(|path| store_path(path.to_str().ok_or("non-UTF-8 build result")?));
    match result {
        Ok(path) => {
            state.phase = Phase::Blocked;
            state.result = Some(path);
            save(dir, &state)?;
            Err(BLOCKED.into())
        }
        Err(e) => {
            state.phase = Phase::Failed;
            save(dir, &state)?;
            Err(e)
        }
    }
}

fn execute(args: &[String], dir: &Path) -> Result<(), String> {
    // Linux root is required for every operation, including status.
    if unsafe { geteuid() } != 0 {
        return Err("boreal-updater must run as root".into());
    }
    match args.first().map(String::as_str) {
        Some("schedule") if args.len() == 4 => {
            let target = Target::new(&args[1], &args[2], &args[3])?;
            prepare_dir(dir)?;
            let _lock = lock(dir)?;
            save(dir, &State { target, phase: Phase::Scheduled, result: None })?;
            println!("Scheduled; start the updater service to prebuild. Activation is not implemented.");
            Ok(())
        }
        Some("status") if args.len() == 1 => {
            prepare_dir(dir)?;
            let state = load(dir)?;
            print!("{}", state.encode());
            if state.phase == Phase::Blocked { println!("{BLOCKED}"); }
            Ok(())
        }
        Some("run") if args.len() == 1 => {
            prepare_dir(dir)?;
            run_worker(dir, &NixBuilder)
        }
        _ => Err("usage: boreal-updater schedule <https-git-repository> <40-digit-commit> <hostname> | status | run".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args_os()
        .skip(1)
        .map(|s| {
            s.into_string()
                .map_err(|_| "arguments must be UTF-8".to_owned())
        })
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(2)
        });
    match execute(&args, Path::new(STATE_DIR)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("boreal-updater: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn target() -> Target {
        Target::new(
            "https://github.com/example/lab.git",
            "0123456789abcdef0123456789ABCDEF01234567",
            "lab-1",
        )
        .unwrap()
    }

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let id = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir =
                env::temp_dir().join(format!("boreal-updater-test-{}-{id}", std::process::id()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            for name in ["lock", "state", "state.tmp"] {
                let path = self.0.join(name);
                if path.exists() {
                    fs::remove_file(path).unwrap();
                }
            }
            for entry in fs::read_dir(&self.0).unwrap() {
                let path = entry.unwrap().path();
                assert!(
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("attempt-")
                );
                fs::remove_dir(path).unwrap();
            }
            fs::remove_dir(&self.0).unwrap();
        }
    }

    struct FakeBuilder {
        calls: Cell<usize>,
        fail: Cell<bool>,
    }
    impl Builder for FakeBuilder {
        fn build(&self, _: &Target, _: &Path) -> Result<PathBuf, String> {
            self.calls.set(self.calls.get() + 1);
            if self.fail.get() {
                Err("build failed".into())
            } else {
                Ok("/nix/store/test-system".into())
            }
        }
    }

    #[test]
    fn test_validates_and_pins_all_target_fields() {
        let t = target();
        assert_eq!(
            t.flake(),
            "git+https://github.com/example/lab.git?rev=0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(
            t.installable(),
            format!(
                "{}#nixosConfigurations.lab-1.config.system.build.toplevel",
                t.flake()
            )
        );
        let rev = &t.rev;
        for repo in [
            "http://example.org/a",
            "https://user:pass@example.org/a",
            "https://example.org/a?rev=other",
            "https://example.org/a#x",
            "https://example.org/\"oops",
            "https://example.org/",
        ] {
            assert!(Target::new(repo, rev, "host").is_err(), "{repo}");
        }
        for bad in ["", "abc", "0123456789abcdef0123456789abcdef0123456g"] {
            assert!(Target::new(&t.repo, bad, "host").is_err());
        }
        for host in [
            "",
            "host.example",
            "-host",
            "host-",
            "host/x",
            "host\nother",
        ] {
            assert!(Target::new(&t.repo, rev, host).is_err(), "{host:?}");
        }
    }

    #[test]
    fn test_nix_commands_pin_source_and_forbid_lock_updates() {
        let target = target();
        let eval = NixBuilder::source_command(&target);
        let args: Vec<_> = eval.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(eval.get_program(), "nix");
        assert!(args.contains(&"--no-update-lock-file"));
        assert!(args.contains(&"--no-write-lock-file"));
        assert_eq!(
            args.last().unwrap(),
            &format!("(builtins.getFlake \"{}\").outPath", target.flake())
        );

        let build = NixBuilder::build_command(&target, Path::new("/tmp/result"));
        let args: Vec<_> = build.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(build.get_program(), "nix");
        assert!(args.contains(&"--no-update-lock-file"));
        assert!(args.contains(&"--no-write-lock-file"));
        assert!(args.windows(2).any(|p| p == ["--out-link", "/tmp/result"]));
        assert_eq!(args.last().unwrap(), &target.installable());
    }

    #[test]
    fn test_state_roundtrip_and_reject_inconsistent_data() {
        let state = State {
            target: target(),
            phase: Phase::Scheduled,
            result: None,
        };
        let text = state.encode();
        let decoded = State::decode(&text).unwrap();
        assert_eq!(decoded.target, state.target);
        assert_eq!(decoded.phase, Phase::Scheduled);
        assert!(State::decode(&text.replace("phase=scheduled", "phase=blocked")).is_err());
        assert!(State::decode(&text.replace("result=", "result=/nix/store/a\nextra=")).is_err());
        assert!(State::decode(&text.replace("result=", "result=/etc/passwd")).is_err());
        assert!(State::decode(&text.replace("version=1", "version=2")).is_err());
    }

    #[test]
    fn test_build_is_durable_and_permanently_blocked_without_activation() {
        let dir = TestDir::new();
        save(
            &dir.0,
            &State {
                target: target(),
                phase: Phase::Scheduled,
                result: None,
            },
        )
        .unwrap();
        let builder = FakeBuilder {
            calls: Cell::new(0),
            fail: Cell::new(false),
        };
        assert_eq!(run_worker(&dir.0, &builder).unwrap_err(), BLOCKED);
        let state = load(&dir.0).unwrap();
        assert_eq!(state.phase, Phase::Blocked);
        assert_eq!(state.result, Some("/nix/store/test-system".into()));
        assert_eq!(run_worker(&dir.0, &builder).unwrap_err(), BLOCKED);
        assert_eq!(builder.calls.get(), 1);
    }

    #[test]
    fn test_failed_or_interrupted_build_can_be_retried() {
        let dir = TestDir::new();
        // A crash leaves building state; it must not be mistaken for success.
        save(
            &dir.0,
            &State {
                target: target(),
                phase: Phase::Building,
                result: None,
            },
        )
        .unwrap();
        let builder = FakeBuilder {
            calls: Cell::new(0),
            fail: Cell::new(true),
        };
        assert_eq!(run_worker(&dir.0, &builder).unwrap_err(), "build failed");
        assert_eq!(load(&dir.0).unwrap().phase, Phase::Failed);
        builder.fail.set(false);
        assert_eq!(run_worker(&dir.0, &builder).unwrap_err(), BLOCKED);
        assert_eq!(load(&dir.0).unwrap().phase, Phase::Blocked);
        assert_eq!(builder.calls.get(), 2);
    }

    #[test]
    fn test_lock_excludes_other_schedulers_and_workers() {
        let dir = TestDir::new();
        let held = lock(&dir.0).unwrap();
        assert!(lock(&dir.0).is_err());
        drop(held);
        assert!(lock(&dir.0).is_ok());
    }
}
