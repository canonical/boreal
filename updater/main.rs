use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use boreal_common::os;
use boreal_common::wire::{self, UPDATER_SOCKET, UpdaterRequest, UpdaterResponse};
use boreal_common::yaml::{self, Value};

const STATE_DIR: &str = "/var/lib/boreal-updater";
const POLICY_PATH: &str = "/etc/boreal/updater.yaml";
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(3600);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub repository: String,
    pub branch: Option<String>,
    pub host: String,
    pub allow_switch: bool,
    pub worker_units: Vec<String>,
    pub broker_socket: PathBuf,
}

impl Policy {
    pub fn load_system() -> Result<Self, String> {
        let bytes = os::read_owned_file(Path::new(POLICY_PATH), Some(0), 0o377, 65536)
            .map_err(|e| format!("read {POLICY_PATH}: {}", e.detail()))?;
        let text =
            String::from_utf8(bytes).map_err(|_| format!("{POLICY_PATH} must be valid UTF-8"))?;
        Self::from_yaml(&text)
    }

    pub fn from_yaml(text: &str) -> Result<Self, String> {
        let doc = yaml::parse(text).map_err(|e| format!("parse updater policy YAML: {e:?}"))?;
        let map = doc.as_map().ok_or("updater.yaml root must be a mapping")?;
        let repo = map
            .get("repository")
            .and_then(Value::as_str)
            .ok_or("updater.yaml missing 'repository'")?;
        validate_repo(repo)?;

        let branch = match map.get("branch") {
            None | Some(Value::Null) => None,
            Some(Value::Str(s)) if s.is_empty() => None,
            Some(Value::Str(s)) => {
                validate_branch(s)?;
                Some(s.clone())
            }
            _ => return Err("updater.yaml 'branch' must be a string or null".into()),
        };

        let host = map
            .get("host")
            .and_then(Value::as_str)
            .ok_or("updater.yaml missing 'host'")?;
        validate_host(host)?;

        let allow_switch = map
            .get("allowSwitch")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut worker_units = Vec::new();
        if let Some(seq) = map.get("workerUnits").and_then(Value::as_seq) {
            for item in seq {
                let unit = item
                    .as_str()
                    .filter(|u| valid_unit_name(u))
                    .ok_or("invalid unit name in updater.yaml workerUnits")?;
                worker_units.push(unit.to_owned());
            }
        }

        let broker_socket = map
            .get("brokerSocket")
            .and_then(Value::as_str)
            .map_or_else(|| PathBuf::from(UPDATER_SOCKET), PathBuf::from);

        Ok(Self {
            repository: repo.to_owned(),
            branch,
            host: host.to_owned(),
            allow_switch,
            worker_units,
            broker_socket,
        })
    }
}

pub fn validate_repo(repo: &str) -> Result<(), String> {
    let rest = repo
        .strip_prefix("https://")
        .ok_or("repository must use https://")?;
    let (host_port, path) = rest
        .split_once('/')
        .ok_or("repository URL must include a path")?;
    let domain = match host_port.split_once(':') {
        Some((h, port)) => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err("invalid port in repository URL".into());
            }
            h
        }
        None => host_port,
    };
    if domain.is_empty()
        || !domain
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
        || path.is_empty()
        || path.contains("..")
        || !path
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/._~-".contains(&c))
    {
        return Err(
            "repository URL must contain only a host and an unescaped path (no credentials, query, or fragment)"
                .into(),
        );
    }
    Ok(())
}

pub fn validate_branch(branch: &str) -> Result<(), String> {
    if branch.is_empty()
        || branch.len() > 200
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with(".lock")
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains("@{")
        || !branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b))
    {
        return Err("invalid reviewed Git branch name".into());
    }
    Ok(())
}

pub fn validate_rev(rev: &str) -> Result<String, String> {
    if rev.len() != 40 || !rev.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("revision must be a full 40-digit Git commit hash".into());
    }
    Ok(rev.to_ascii_lowercase())
}

pub fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty()
        || host.len() > 63
        || !host.as_bytes()[0].is_ascii_alphanumeric()
        || !host.as_bytes()[host.len() - 1].is_ascii_alphanumeric()
        || !host.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return Err("host must be a single DNS-style hostname".into());
    }
    Ok(())
}

fn valid_unit_name(u: &str) -> bool {
    u.ends_with(".service")
        && u.len() <= 128
        && u.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    repo: String,
    branch: Option<String>,
    rev: String,
    host: String,
}

impl Target {
    fn from_policy_and_rev(policy: &Policy, rev: &str) -> Result<Self, String> {
        Ok(Self {
            repo: policy.repository.clone(),
            branch: policy.branch.clone(),
            rev: validate_rev(rev)?,
            host: policy.host.clone(),
        })
    }

    fn verify_matches_policy(&self, policy: &Policy) -> Result<(), String> {
        if self.repo != policy.repository {
            return Err(format!(
                "scheduled repository '{}' does not match configured '{}'",
                self.repo, policy.repository
            ));
        }
        if self.branch != policy.branch {
            return Err("scheduled branch does not match configured reviewed branch".into());
        }
        if self.host != policy.host {
            return Err(format!(
                "scheduled host '{}' does not match configured '{}'",
                self.host, policy.host
            ));
        }
        Ok(())
    }

    fn flake(&self) -> String {
        match &self.branch {
            Some(branch) => format!("git+{}?ref=refs/heads/{branch}&rev={}", self.repo, self.rev),
            None => format!("git+{}?rev={}", self.repo, self.rev),
        }
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
    Prebuilt,
    Draining,
    Activating,
    Activated,
    Failed,
    Blocked,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Building => "building",
            Self::Prebuilt => "prebuilt",
            Self::Draining => "draining",
            Self::Activating => "activating",
            Self::Activated => "activated",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }

    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "scheduled" => Ok(Self::Scheduled),
            "building" => Ok(Self::Building),
            "prebuilt" => Ok(Self::Prebuilt),
            "draining" => Ok(Self::Draining),
            "activating" => Ok(Self::Activating),
            "activated" => Ok(Self::Activated),
            "failed" => Ok(Self::Failed),
            "blocked" => Ok(Self::Blocked),
            _ => Err(format!("invalid state phase '{s}'")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    update_id: String,
    target: Target,
    phase: Phase,
    result: Option<PathBuf>,
    gc_root: Option<PathBuf>,
    reason: String,
}

impl State {
    fn encode(&self) -> String {
        let clean_reason: String = self
            .reason
            .chars()
            .map(|c| if matches!(c, '\n' | '\r') { ' ' } else { c })
            .collect();
        format!(
            "version=2\nphase={}\nupdate_id={}\nrepo={}\nbranch={}\nrev={}\nhost={}\nresult={}\ngc_root={}\nreason={}\n",
            self.phase.name(),
            self.update_id,
            self.target.repo,
            self.target.branch.as_deref().unwrap_or(""),
            self.target.rev,
            self.target.host,
            self.result.as_deref().and_then(Path::to_str).unwrap_or(""),
            self.gc_root.as_deref().and_then(Path::to_str).unwrap_or(""),
            clean_reason,
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
        if field(&mut lines, "version=")? != "2" {
            return Err("unsupported state version".into());
        }
        let phase = Phase::parse(field(&mut lines, "phase=")?)?;
        let update_id = field(&mut lines, "update_id=")?;
        if update_id.is_empty()
            || !update_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err("invalid update_id in state".into());
        }
        let repo = field(&mut lines, "repo=")?;
        validate_repo(repo)?;
        let raw_branch = field(&mut lines, "branch=")?;
        let branch = if raw_branch.is_empty() {
            None
        } else {
            validate_branch(raw_branch)?;
            Some(raw_branch.to_owned())
        };
        let rev = validate_rev(field(&mut lines, "rev=")?)?;
        let host = field(&mut lines, "host=")?;
        validate_host(host)?;
        let raw_result = field(&mut lines, "result=")?;
        let raw_gc_root = field(&mut lines, "gc_root=")?;
        let reason = field(&mut lines, "reason=")?;
        if lines.next().is_some() {
            return Err("unexpected trailing fields in state".into());
        }
        let result = if raw_result.is_empty() {
            None
        } else {
            Some(store_path(raw_result)?)
        };
        let gc_root = if raw_gc_root.is_empty() {
            None
        } else {
            Some(PathBuf::from(raw_gc_root))
        };
        if matches!(
            phase,
            Phase::Prebuilt | Phase::Draining | Phase::Activating | Phase::Activated
        ) && (result.is_none() || gc_root.is_none())
        {
            return Err("state phase requires prebuilt result and gc_root".into());
        }
        Ok(Self {
            update_id: update_id.to_owned(),
            target: Target {
                repo: repo.to_owned(),
                branch,
                rev,
                host: host.to_owned(),
            },
            phase,
            result,
            gc_root,
            reason: reason.to_owned(),
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

fn lock(dir: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("lock"))
        .map_err(|e| format!("open lock: {e}"))?;
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
        return Err(format!(
            "another updater operation is in progress: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(file)
}

fn load_opt(dir: &Path) -> Result<Option<State>, String> {
    match fs::read_to_string(dir.join("state")) {
        Ok(text) => State::decode(&text).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read state: {e}")),
    }
}

fn load(dir: &Path) -> Result<State, String> {
    load_opt(dir)?.ok_or_else(|| "no update has been scheduled".into())
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

fn new_update_id() -> Result<String, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    Ok(format!("upd-{nanos}-{}", std::process::id()))
}

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

trait Environment {
    fn prebuild(&self, target: &Target, attempt: &Path) -> Result<PathBuf, String>;
    fn verify_prebuilt(&self, gc_root: &Path, expected_result: &Path) -> Result<(), String>;
    fn broker_call(&self, req: &UpdaterRequest) -> Result<UpdaterResponse, String>;
    fn stop_workers(&self, units: &[String]) -> Result<(), String>;
    fn start_workers(&self, units: &[String]) -> Result<(), String>;
    fn activate_closure(&self, result: &Path) -> Result<(), String>;
    fn current_system_matches(&self, expected: &Path) -> Result<bool, String>;
    fn wait_core_services(&self) -> Result<(), String>;
    fn drain_timeout(&self) -> Duration {
        DEFAULT_DRAIN_TIMEOUT
    }
    fn poll_interval(&self) -> Duration {
        DEFAULT_POLL_INTERVAL
    }
}

struct LiveEnv<'a> {
    policy: &'a Policy,
}

impl LiveEnv<'_> {
    fn git_fetch_branch_cmd(git_dir: &Path, repo: &str, branch: &str) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(git_dir).args([
            "fetch",
            "--quiet",
            "--no-tags",
            "--",
            repo,
            &format!("+refs/heads/{branch}:refs/remotes/origin/reviewed"),
        ]);
        cmd
    }

    fn git_ancestor_cmd(git_dir: &Path, rev: &str) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(git_dir).args([
            "merge-base",
            "--is-ancestor",
            rev,
            "refs/remotes/origin/reviewed",
        ]);
        cmd
    }

    fn nix_source_command(flake_ref: &str) -> Command {
        let expr = format!("(builtins.getFlake \"{flake_ref}\").outPath");
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

    fn nix_build_command(installable: &str, result_link: &Path) -> Command {
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
            .arg(installable);
        command
    }

    fn verify_commit_in_repo(target: &Target, git_dir: &Path) -> Result<(), String> {
        DirBuilder::new()
            .mode(0o700)
            .create(git_dir)
            .map_err(|e| format!("create git dir: {e}"))?;
        let init = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(git_dir)
            .status()
            .map_err(|e| format!("start git init: {e}"))?;
        if !init.success() {
            return Err("git init --bare failed".into());
        }

        if let Some(branch) = &target.branch {
            let fetch = Self::git_fetch_branch_cmd(git_dir, &target.repo, branch)
                .output()
                .map_err(|e| format!("start git fetch: {e}"))?;
            if !fetch.status.success() {
                return Err(format!(
                    "failed to fetch reviewed branch '{branch}' from '{}': {}",
                    target.repo,
                    String::from_utf8_lossy(&fetch.stderr).trim()
                ));
            }
            let verify_obj = Command::new("git")
                .arg("-C")
                .arg(git_dir)
                .args([
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{commit}}", target.rev),
                ])
                .output()
                .map_err(|e| format!("git rev-parse: {e}"))?;
            if !verify_obj.status.success()
                || String::from_utf8_lossy(&verify_obj.stdout).trim() != target.rev
            {
                return Err(format!(
                    "commit {} is not reachable from reviewed branch '{branch}'",
                    target.rev
                ));
            }
            let anc = Self::git_ancestor_cmd(git_dir, &target.rev)
                .status()
                .map_err(|e| format!("git merge-base: {e}"))?;
            if !anc.success() {
                return Err(format!(
                    "commit {} is not an ancestor of reviewed branch '{branch}'",
                    target.rev
                ));
            }
        } else {
            let fetch = Command::new("git")
                .arg("-C")
                .arg(git_dir)
                .args([
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--depth=1",
                    "--",
                    &target.repo,
                    &target.rev,
                ])
                .output()
                .map_err(|e| format!("start git fetch: {e}"))?;
            if !fetch.status.success() {
                return Err(format!(
                    "commit {} not found in '{}': {}",
                    target.rev,
                    target.repo,
                    String::from_utf8_lossy(&fetch.stderr).trim()
                ));
            }
            let verify = Command::new("git")
                .arg("-C")
                .arg(git_dir)
                .args(["rev-parse", "--verify", "FETCH_HEAD^{commit}"])
                .output()
                .map_err(|e| format!("git rev-parse: {e}"))?;
            if !verify.status.success()
                || String::from_utf8_lossy(&verify.stdout).trim() != target.rev
            {
                return Err("fetched revision did not match requested commit".into());
            }
        }
        Ok(())
    }
}

pub fn validate_lockfile_hygiene(lock_text: &str) -> Result<(), String> {
    if lock_text.contains("file://")
        || lock_text.contains("\"type\": \"path\"")
        || lock_text.contains("\"type\":\"path\"")
    {
        return Err(
            "committed flake.lock must not contain local file:// or path input overrides".into(),
        );
    }
    Ok(())
}

impl Environment for LiveEnv<'_> {
    fn prebuild(&self, target: &Target, attempt: &Path) -> Result<PathBuf, String> {
        let git_dir = attempt.join("git");
        Self::verify_commit_in_repo(target, &git_dir)?;

        let nix_home = attempt.join("home");
        DirBuilder::new()
            .mode(0o700)
            .create(&nix_home)
            .map_err(|e| format!("create nix cache dir: {e}"))?;

        let flake_ref = target.flake();
        let output = Self::nix_source_command(&flake_ref)
            .env("HOME", &nix_home)
            .env("XDG_CACHE_HOME", &nix_home)
            .output()
            .map_err(|e| format!("start nix eval: {e}"))?;
        if !output.status.success() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            return Err(format!("nix source resolution failed: {}", output.status));
        }
        let source =
            String::from_utf8(output.stdout).map_err(|_| "nix returned a non-UTF-8 source path")?;
        let source = store_path(source.trim())?;
        let lock_path = source.join("flake.lock");
        let lock_meta = fs::symlink_metadata(&lock_path)
            .map_err(|e| format!("pinned source must contain flake.lock: {e}"))?;
        if !lock_meta.is_file() {
            return Err("pinned flake.lock must be a regular file".into());
        }
        let lock_text =
            fs::read_to_string(&lock_path).map_err(|e| format!("read flake.lock: {e}"))?;
        validate_lockfile_hygiene(&lock_text)?;

        let result_link = attempt.join("result");
        let status = Self::nix_build_command(&target.installable(), &result_link)
            .env("HOME", &nix_home)
            .env("XDG_CACHE_HOME", &nix_home)
            .status()
            .map_err(|e| format!("start nix build: {e}"))?;
        if !status.success() {
            return Err(format!("nix build failed: {status}"));
        }
        let path =
            fs::read_link(&result_link).map_err(|e| format!("read build result link: {e}"))?;
        store_path(path.to_str().ok_or("non-UTF-8 build result")?)
    }

    fn verify_prebuilt(&self, gc_root: &Path, expected_result: &Path) -> Result<(), String> {
        let linked = fs::read_link(gc_root)
            .map_err(|e| format!("read prebuilt GC root {}: {e}", gc_root.display()))?;
        if linked != expected_result {
            return Err("prebuilt GC root no longer matches recorded store path".into());
        }
        let switch_bin = expected_result.join("bin/switch-to-configuration");
        let meta = fs::metadata(&switch_bin)
            .map_err(|e| format!("prebuilt closure missing {}: {e}", switch_bin.display()))?;
        if !meta.is_file() {
            return Err("bin/switch-to-configuration is not a regular file".into());
        }
        Ok(())
    }

    fn broker_call(&self, req: &UpdaterRequest) -> Result<UpdaterResponse, String> {
        let mut stream = UnixStream::connect(&self.policy.broker_socket).map_err(|e| {
            format!(
                "connect broker {}: {e}",
                self.policy.broker_socket.display()
            )
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        wire::write_frame(&mut stream, &req.encode()).map_err(|e| e.detail())?;
        let raw = wire::read_frame(&mut stream).map_err(|e| e.detail())?;
        UpdaterResponse::decode(&raw).map_err(|e| e.detail())
    }

    fn stop_workers(&self, units: &[String]) -> Result<(), String> {
        if units.is_empty() {
            return Ok(());
        }
        let status = Command::new("/run/current-system/sw/bin/systemctl")
            .arg("stop")
            .args(units)
            .status()
            .map_err(|e| format!("systemctl stop workers: {e}"))?;
        if !status.success() {
            return Err(format!("systemctl stop workers exited with {status}"));
        }
        Ok(())
    }

    fn start_workers(&self, units: &[String]) -> Result<(), String> {
        if units.is_empty() {
            return Ok(());
        }
        let status = Command::new("/run/current-system/sw/bin/systemctl")
            .arg("start")
            .args(units)
            .status()
            .map_err(|e| format!("systemctl start workers: {e}"))?;
        if !status.success() {
            return Err(format!("systemctl start workers exited with {status}"));
        }
        Ok(())
    }

    fn activate_closure(&self, result: &Path) -> Result<(), String> {
        let profile_status = Command::new("/run/current-system/sw/bin/nix-env")
            .env("HOME", "/root")
            .args(["-p", "/nix/var/nix/profiles/system", "--set"])
            .arg(result)
            .status()
            .map_err(|e| format!("start nix-env --set: {e}"))?;
        if !profile_status.success() {
            return Err(format!("nix-env --set failed: {profile_status}"));
        }
        let switch_bin = result.join("bin/switch-to-configuration");
        let switch_status = Command::new(&switch_bin)
            .arg("switch")
            .status()
            .map_err(|e| format!("start {}: {e}", switch_bin.display()))?;
        if !switch_status.success() {
            return Err(format!("switch-to-configuration failed: {switch_status}"));
        }
        Ok(())
    }

    fn current_system_matches(&self, expected: &Path) -> Result<bool, String> {
        let cur = fs::canonicalize("/run/current-system")
            .map_err(|e| format!("canonicalize /run/current-system: {e}"))?;
        let exp = fs::canonicalize(expected)
            .map_err(|e| format!("canonicalize {}: {e}", expected.display()))?;
        Ok(cur == exp)
    }

    fn wait_core_services(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let ok = Command::new("/run/current-system/sw/bin/systemctl")
                .args([
                    "is-active",
                    "--quiet",
                    "temporal.service",
                    "boreal-broker.service",
                ])
                .status()
                .is_ok_and(|s| s.success());
            if ok && self.broker_call(&UpdaterRequest::GateStatus).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    "temporal.service or boreal-broker.service did not recover after switch".into(),
                );
            }
            thread::sleep(Duration::from_millis(300));
        }
    }
}

fn schedule_locked(dir: &Path, policy: &Policy, rev: &str) -> Result<State, String> {
    if let Some(existing) = load_opt(dir)?
        && matches!(
            existing.phase,
            Phase::Building | Phase::Draining | Phase::Activating
        )
    {
        return Err(format!(
            "cannot replace update '{}' while phase is '{}'",
            existing.update_id,
            existing.phase.name()
        ));
    }
    let target = Target::from_policy_and_rev(policy, rev)?;
    let state = State {
        update_id: new_update_id()?,
        target,
        phase: Phase::Scheduled,
        result: None,
        gc_root: None,
        reason: String::new(),
    };
    save(dir, &state)?;
    Ok(state)
}

fn prebuild_locked(dir: &Path, policy: &Policy, env: &impl Environment) -> Result<State, String> {
    let mut state = load(dir)?;
    state.target.verify_matches_policy(policy)?;
    if state.phase == Phase::Blocked {
        return Err(format!("activation blocked: {}", state.reason));
    }
    if matches!(
        state.phase,
        Phase::Draining | Phase::Activating | Phase::Activated
    ) {
        return Err(format!(
            "cannot prebuild while update is in phase '{}'",
            state.phase.name()
        ));
    }
    if state.phase == Phase::Prebuilt {
        if let (Some(root), Some(res)) = (&state.gc_root, &state.result) {
            env.verify_prebuilt(root, res)?;
        }
        return Ok(state);
    }

    state.phase = Phase::Building;
    state.result = None;
    state.gc_root = None;
    state.reason.clear();
    save(dir, &state)?;

    let attempt = attempt_dir(dir)?;
    match env.prebuild(&state.target, &attempt) {
        Ok(path) => {
            let gc_root = attempt.join("result");
            state.result = Some(path.clone());
            state.gc_root = Some(gc_root.clone());
            if !policy.allow_switch {
                state.phase = Phase::Blocked;
                state.reason = "system.switch.enable is false on this configuration".into();
                save(dir, &state)?;
                return Err(format!("activation blocked: {}", state.reason));
            }
            if let Err(err) = env.verify_prebuilt(&gc_root, &path) {
                state.phase = Phase::Blocked;
                state.reason = err.clone();
                save(dir, &state)?;
                return Err(format!("activation blocked: {err}"));
            }
            state.phase = Phase::Prebuilt;
            save(dir, &state)?;
            Ok(state)
        }
        Err(err) => {
            state.phase = Phase::Failed;
            state.reason = err.clone();
            save(dir, &state)?;
            Err(err)
        }
    }
}

fn block_update(dir: &Path, state: &mut State, reason: impl Into<String>) -> Result<(), String> {
    let msg = reason.into();
    state.phase = Phase::Blocked;
    state.reason = msg.clone();
    save(dir, state)?;
    Err(format!("activation blocked: {msg}"))
}

fn run_locked(dir: &Path, policy: &Policy, env: &impl Environment) -> Result<(), String> {
    let current = load(dir)?;
    current.target.verify_matches_policy(policy)?;

    match current.phase {
        Phase::Activated => return Ok(()),
        Phase::Blocked => return Err(format!("activation blocked: {}", current.reason)),
        Phase::Activating => {
            return Err(
                "previous update was interrupted during activation; run 'boreal-updater recover' first"
                    .into(),
            );
        }
        Phase::Scheduled | Phase::Failed | Phase::Building => {
            prebuild_locked(dir, policy, env)?;
        }
        Phase::Prebuilt | Phase::Draining => {}
    }

    let mut state = load(dir)?;
    if !policy.allow_switch {
        return block_update(
            dir,
            &mut state,
            "system.switch.enable is false on this configuration",
        );
    }
    let (Some(gc_root), Some(result_path)) = (state.gc_root.clone(), state.result.clone()) else {
        return block_update(dir, &mut state, "missing prebuilt store closure");
    };
    if let Err(err) = env.verify_prebuilt(&gc_root, &result_path) {
        return block_update(dir, &mut state, err);
    }

    state.phase = Phase::Draining;
    state.reason = "closing admission barrier and pausing schedules".into();
    save(dir, &state)?;

    match env.broker_call(&UpdaterRequest::CloseGate {
        update_id: state.update_id.clone(),
        rev: state.target.rev.clone(),
    }) {
        Ok(UpdaterResponse::GateClosed) => {}
        Ok(UpdaterResponse::Error(msg)) | Err(msg) => {
            return block_update(dir, &mut state, format!("close admission gate: {msg}"));
        }
        Ok(other) => {
            return block_update(
                dir,
                &mut state,
                format!("unexpected broker response on CloseGate: {other:?}"),
            );
        }
    }

    let deadline = Instant::now() + env.drain_timeout();
    let mut quiescent = false;
    for round in 0..3 {
        loop {
            match env.broker_call(&UpdaterRequest::CheckDrain {
                update_id: state.update_id.clone(),
            }) {
                Ok(UpdaterResponse::DrainQuiescent) => break,
                Ok(UpdaterResponse::DrainBusy { detail, .. }) => {
                    if Instant::now() >= deadline {
                        return block_update(
                            dir,
                            &mut state,
                            format!("drain timed out while work remained active ({detail})"),
                        );
                    }
                    state.reason = detail;
                    let _ = save(dir, &state);
                    thread::sleep(env.poll_interval());
                }
                Ok(UpdaterResponse::Error(msg)) | Err(msg) => {
                    return block_update(dir, &mut state, format!("uncertain drain state: {msg}"));
                }
                Ok(other) => {
                    return block_update(
                        dir,
                        &mut state,
                        format!("unexpected drain response: {other:?}"),
                    );
                }
            }
        }

        if let Err(err) = env.stop_workers(&policy.worker_units) {
            return block_update(dir, &mut state, format!("stop workers: {err}"));
        }

        match env.broker_call(&UpdaterRequest::CheckDrain {
            update_id: state.update_id.clone(),
        }) {
            Ok(UpdaterResponse::DrainQuiescent) => {
                quiescent = true;
                break;
            }
            Ok(UpdaterResponse::DrainBusy { .. }) if round < 2 => {
                if let Err(err) = env.start_workers(&policy.worker_units) {
                    return block_update(dir, &mut state, format!("restart workers: {err}"));
                }
            }
            Ok(UpdaterResponse::DrainBusy { detail, .. }) => {
                return block_update(
                    dir,
                    &mut state,
                    format!("work remained active after stopping workers: {detail}"),
                );
            }
            Ok(UpdaterResponse::Error(msg)) | Err(msg) => {
                return block_update(
                    dir,
                    &mut state,
                    format!("post-stop drain check uncertain: {msg}"),
                );
            }
            Ok(other) => {
                return block_update(
                    dir,
                    &mut state,
                    format!("unexpected post-stop drain response: {other:?}"),
                );
            }
        }
    }

    if !quiescent {
        return block_update(dir, &mut state, "failed to establish host quiescence");
    }

    if let Err(err) = env.verify_prebuilt(&gc_root, &result_path) {
        return block_update(dir, &mut state, err);
    }

    state.phase = Phase::Activating;
    state.reason = "switching NixOS configuration to prebuilt closure".into();
    save(dir, &state)?;

    if let Err(err) = env.activate_closure(&result_path) {
        return block_update(dir, &mut state, format!("activation failed: {err}"));
    }

    match env.current_system_matches(&result_path) {
        Ok(true) => {}
        Ok(false) => {
            return block_update(
                dir,
                &mut state,
                "/run/current-system does not match prebuilt closure after switch",
            );
        }
        Err(err) => return block_update(dir, &mut state, err),
    }

    if let Err(err) = env.wait_core_services() {
        return block_update(dir, &mut state, err);
    }

    match env.broker_call(&UpdaterRequest::ReopenGate {
        update_id: state.update_id.clone(),
    }) {
        Ok(UpdaterResponse::GateReopened) => {}
        Ok(UpdaterResponse::Error(msg)) | Err(msg) => {
            return block_update(
                dir,
                &mut state,
                format!("switched but failed to reopen gate: {msg}"),
            );
        }
        Ok(other) => {
            return block_update(
                dir,
                &mut state,
                format!("unexpected ReopenGate response: {other:?}"),
            );
        }
    }

    state.phase = Phase::Activated;
    state.reason = "activated prebuilt downstream revision".into();
    save(dir, &state)
}

fn recover_locked(dir: &Path, policy: &Policy, env: &impl Environment) -> Result<(), String> {
    let Some(mut state) = load_opt(dir)? else {
        return Ok(());
    };
    match state.phase {
        Phase::Activating => {
            let Some(result_path) = state.result.clone() else {
                return block_update(dir, &mut state, "missing result path during recovery");
            };
            match env.current_system_matches(&result_path) {
                Ok(true) => {
                    env.wait_core_services()?;
                    match env.broker_call(&UpdaterRequest::ReopenGate {
                        update_id: state.update_id.clone(),
                    }) {
                        Ok(UpdaterResponse::GateReopened) => {
                            state.phase = Phase::Activated;
                            state.reason = "recovered completed system switch".into();
                            save(dir, &state)
                        }
                        Ok(UpdaterResponse::Error(msg)) | Err(msg) => {
                            block_update(dir, &mut state, format!("recovery reopen failed: {msg}"))
                        }
                        Ok(other) => block_update(
                            dir,
                            &mut state,
                            format!("unexpected recovery response: {other:?}"),
                        ),
                    }
                }
                Ok(false) => block_update(
                    dir,
                    &mut state,
                    "interrupted during activation before switch completed; safety uncertain",
                ),
                Err(err) => block_update(dir, &mut state, err),
            }
        }
        Phase::Draining => {
            env.wait_core_services()?;
            env.start_workers(&policy.worker_units)?;
            match env.broker_call(&UpdaterRequest::ReopenGate {
                update_id: state.update_id.clone(),
            }) {
                Ok(UpdaterResponse::GateReopened) => {
                    state.phase = Phase::Prebuilt;
                    state.reason = "recovered interrupted drain; admission gate reopened".into();
                    save(dir, &state)
                }
                Ok(UpdaterResponse::Error(msg)) | Err(msg) => {
                    block_update(dir, &mut state, format!("recovery reopen failed: {msg}"))
                }
                Ok(other) => block_update(
                    dir,
                    &mut state,
                    format!("unexpected recovery response: {other:?}"),
                ),
            }
        }
        _ => Ok(()),
    }
}

const USAGE: &str = "usage: boreal-updater schedule [<https-repo>] <40-digit-commit> [<host>] | prebuild | run | recover | status";

fn execute(args: &[String], dir: &Path) -> Result<(), String> {
    if os::current_euid() != 0 {
        return Err("boreal-updater must run as root".into());
    }
    match args.first().map(String::as_str) {
        Some("schedule") if args.len() == 2 || args.len() == 4 => {
            let policy = Policy::load_system()?;
            let rev = if args.len() == 2 {
                &args[1]
            } else {
                if args[1] != policy.repository {
                    return Err(format!(
                        "repository '{}' is not permitted; configured repository is '{}'",
                        args[1], policy.repository
                    ));
                }
                if args[3] != policy.host {
                    return Err(format!(
                        "host '{}' is not permitted; configured host is '{}'",
                        args[3], policy.host
                    ));
                }
                &args[2]
            };
            prepare_dir(dir)?;
            let _lock = lock(dir)?;
            let state = schedule_locked(dir, &policy, rev)?;
            println!(
                "Scheduled {} ({}) for {}.",
                state.target.rev, state.update_id, state.target.host
            );
            Ok(())
        }
        Some("prebuild") if args.len() == 1 => {
            let policy = Policy::load_system()?;
            prepare_dir(dir)?;
            let _lock = lock(dir)?;
            let env = LiveEnv { policy: &policy };
            let state = prebuild_locked(dir, &policy, &env)?;
            println!(
                "Prebuilt {} -> {}",
                state.target.rev,
                state
                    .result
                    .as_deref()
                    .map_or("", |p| p.to_str().unwrap_or(""))
            );
            Ok(())
        }
        Some("run") if args.len() == 1 => {
            let policy = Policy::load_system()?;
            prepare_dir(dir)?;
            let _lock = lock(dir)?;
            let env = LiveEnv { policy: &policy };
            run_locked(dir, &policy, &env)?;
            let state = load(dir)?;
            println!(
                "Activated {} -> {}",
                state.target.rev,
                state
                    .result
                    .as_deref()
                    .map_or("", |p| p.to_str().unwrap_or(""))
            );
            Ok(())
        }
        Some("recover") if args.len() == 1 => {
            let policy = Policy::load_system()?;
            prepare_dir(dir)?;
            let _lock = lock(dir)?;
            let env = LiveEnv { policy: &policy };
            recover_locked(dir, &policy, &env)?;
            println!("Recovery check completed.");
            Ok(())
        }
        Some("status") if args.len() == 1 => {
            prepare_dir(dir)?;
            let state = load(dir)?;
            print!("{}", state.encode());
            Ok(())
        }
        _ => Err(USAGE.into()),
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
    use std::cell::{Cell, RefCell};
    use std::os::unix::fs::symlink;

    fn sample_policy(allow_switch: bool) -> Policy {
        Policy::from_yaml(&format!(
            "\
repository: https://github.com/example/demo-lab.git
branch: downstreams/demo-lab
host: lab-1
allowSwitch: {allow_switch}
workerUnits:
- boreal-worker-alice-greet.service
- boreal-worker-alice-count.service
"
        ))
        .unwrap()
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let id = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir =
                env::temp_dir().join(format!("boreal-updater-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct FakeEnv {
        prebuilt_path: PathBuf,
        fail_build: Cell<bool>,
        drain_polls_before_quiescent: Cell<usize>,
        fail_drain: Cell<bool>,
        fail_switch: Cell<bool>,
        switched: Cell<bool>,
        gate_open: Cell<bool>,
        workers_stopped: Cell<bool>,
        events: RefCell<Vec<&'static str>>,
    }

    impl FakeEnv {
        fn new() -> Self {
            Self {
                prebuilt_path: PathBuf::from("/nix/store/0123456789abcdef-nixos-system-lab-1"),
                fail_build: Cell::new(false),
                drain_polls_before_quiescent: Cell::new(1),
                fail_drain: Cell::new(false),
                fail_switch: Cell::new(false),
                switched: Cell::new(false),
                gate_open: Cell::new(true),
                workers_stopped: Cell::new(false),
                events: RefCell::new(Vec::new()),
            }
        }
    }

    impl Environment for FakeEnv {
        fn prebuild(&self, _: &Target, attempt: &Path) -> Result<PathBuf, String> {
            self.events.borrow_mut().push("prebuild");
            if self.fail_build.get() {
                return Err("build failed".into());
            }
            symlink(&self.prebuilt_path, attempt.join("result")).unwrap();
            Ok(self.prebuilt_path.clone())
        }

        fn verify_prebuilt(&self, gc_root: &Path, expected_result: &Path) -> Result<(), String> {
            let target = fs::read_link(gc_root).map_err(|e| e.to_string())?;
            if target != expected_result {
                return Err("tampered gc_root".into());
            }
            Ok(())
        }

        fn broker_call(&self, req: &UpdaterRequest) -> Result<UpdaterResponse, String> {
            match req {
                UpdaterRequest::GateStatus => Ok(UpdaterResponse::GateStatus {
                    open: self.gate_open.get(),
                    update_id: None,
                    rev: None,
                }),
                UpdaterRequest::CloseGate { .. } => {
                    self.events.borrow_mut().push("close_gate");
                    self.gate_open.set(false);
                    Ok(UpdaterResponse::GateClosed)
                }
                UpdaterRequest::CheckDrain { .. } => {
                    self.events.borrow_mut().push("check_drain");
                    if self.fail_drain.get() {
                        return Ok(UpdaterResponse::Error("postgres unreachable".into()));
                    }
                    let rem = self.drain_polls_before_quiescent.get();
                    if rem > 0 {
                        self.drain_polls_before_quiescent.set(rem - 1);
                        Ok(UpdaterResponse::DrainBusy {
                            active_workflows: 1,
                            active_leases: 1,
                            detail: "alice/run-1".into(),
                        })
                    } else {
                        Ok(UpdaterResponse::DrainQuiescent)
                    }
                }
                UpdaterRequest::ReopenGate { .. } => {
                    self.events.borrow_mut().push("reopen_gate");
                    self.gate_open.set(true);
                    Ok(UpdaterResponse::GateReopened)
                }
            }
        }

        fn stop_workers(&self, _: &[String]) -> Result<(), String> {
            self.events.borrow_mut().push("stop_workers");
            self.workers_stopped.set(true);
            Ok(())
        }

        fn start_workers(&self, _: &[String]) -> Result<(), String> {
            self.events.borrow_mut().push("start_workers");
            self.workers_stopped.set(false);
            Ok(())
        }

        fn activate_closure(&self, result: &Path) -> Result<(), String> {
            assert_eq!(result, self.prebuilt_path);
            self.events.borrow_mut().push("activate");
            if self.fail_switch.get() {
                return Err("switch error".into());
            }
            self.switched.set(true);
            Ok(())
        }

        fn current_system_matches(&self, _: &Path) -> Result<bool, String> {
            Ok(self.switched.get())
        }

        fn wait_core_services(&self) -> Result<(), String> {
            Ok(())
        }

        fn drain_timeout(&self) -> Duration {
            Duration::from_secs(2)
        }

        fn poll_interval(&self) -> Duration {
            Duration::from_millis(5)
        }
    }

    #[test]
    fn test_validates_policy_git_ancestry_commands_and_lockfile_hygiene() {
        let policy = sample_policy(true);
        let t = Target::from_policy_and_rev(&policy, "0123456789abcdef0123456789ABCDEF01234567")
            .unwrap();
        assert_eq!(
            t.flake(),
            "git+https://github.com/example/demo-lab.git?ref=refs/heads/downstreams/demo-lab&rev=0123456789abcdef0123456789abcdef01234567"
        );
        let mut wrong_repo = policy.clone();
        wrong_repo.repository = "https://github.com/evil/fork.git".into();
        assert!(t.verify_matches_policy(&wrong_repo).is_err());

        let fetch = LiveEnv::git_fetch_branch_cmd(
            Path::new("/tmp/g"),
            &policy.repository,
            policy.branch.as_deref().unwrap(),
        );
        let f_args: Vec<_> = fetch.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(f_args.contains(&"+refs/heads/downstreams/demo-lab:refs/remotes/origin/reviewed"));

        let eval = LiveEnv::nix_source_command(&t.flake());
        let e_args: Vec<_> = eval.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(e_args.contains(&"--no-update-lock-file"));
        assert!(e_args.contains(&"--no-write-lock-file"));

        assert!(
            validate_lockfile_hygiene(r#"{"nodes":{"boreal":{"locked":{"type":"github"}}}}"#)
                .is_ok()
        );
        assert!(
            validate_lockfile_hygiene(r#"{"nodes":{"boreal":{"locked":{"url":"file:///tmp/x"}}}}"#)
                .is_err()
        );
        assert!(
            validate_lockfile_hygiene(r#"{"nodes":{"boreal":{"locked":{"type":"path"}}}}"#)
                .is_err()
        );
    }

    #[test]
    fn test_blocks_when_allow_switch_is_disabled() {
        let dir = TempDir::new();
        let policy = sample_policy(false);
        let env = FakeEnv::new();
        schedule_locked(&dir.0, &policy, "0123456789abcdef0123456789abcdef01234567").unwrap();
        let err = run_locked(&dir.0, &policy, &env).unwrap_err();
        assert!(err.contains("activation blocked"));
        assert_eq!(load(&dir.0).unwrap().phase, Phase::Blocked);
        assert!(env.gate_open.get());
        assert!(!env.switched.get());
    }

    #[test]
    fn test_full_admission_barrier_drain_stop_recheck_activate_and_reopen() {
        let dir = TempDir::new();
        let policy = sample_policy(true);
        let env = FakeEnv::new();
        schedule_locked(&dir.0, &policy, "0123456789abcdef0123456789abcdef01234567").unwrap();
        run_locked(&dir.0, &policy, &env).unwrap();

        let state = load(&dir.0).unwrap();
        assert_eq!(state.phase, Phase::Activated);
        assert_eq!(state.result.as_deref(), Some(env.prebuilt_path.as_path()));
        assert!(env.switched.get());
        assert!(env.gate_open.get());
        assert_eq!(
            *env.events.borrow(),
            vec![
                "prebuild",
                "close_gate",
                "check_drain",
                "check_drain",
                "stop_workers",
                "check_drain",
                "activate",
                "reopen_gate"
            ]
        );
    }

    #[test]
    fn test_uncertain_drain_blocks_activation_and_keeps_gate_closed() {
        let dir = TempDir::new();
        let policy = sample_policy(true);
        let env = FakeEnv::new();
        env.fail_drain.set(true);
        schedule_locked(&dir.0, &policy, "0123456789abcdef0123456789abcdef01234567").unwrap();
        assert!(run_locked(&dir.0, &policy, &env).is_err());
        assert_eq!(load(&dir.0).unwrap().phase, Phase::Blocked);
        assert!(!env.switched.get());
        assert!(!env.gate_open.get());
    }

    #[test]
    fn test_interrupted_update_recovery_handles_draining_and_activating_states() {
        let dir = TempDir::new();
        let policy = sample_policy(true);
        let env = FakeEnv::new();
        schedule_locked(&dir.0, &policy, "0123456789abcdef0123456789abcdef01234567").unwrap();
        let mut st = prebuild_locked(&dir.0, &policy, &env).unwrap();

        // Simulate crash mid-Draining with gate closed and workers stopped.
        st.phase = Phase::Draining;
        env.gate_open.set(false);
        env.workers_stopped.set(true);
        save(&dir.0, &st).unwrap();
        recover_locked(&dir.0, &policy, &env).unwrap();
        assert_eq!(load(&dir.0).unwrap().phase, Phase::Prebuilt);
        assert!(env.gate_open.get());
        assert!(!env.workers_stopped.get());

        // Simulate crash mid-Activating BEFORE switch completed -> must fail closed (Blocked)!
        st.phase = Phase::Activating;
        env.gate_open.set(false);
        env.switched.set(false);
        save(&dir.0, &st).unwrap();
        assert!(recover_locked(&dir.0, &policy, &env).is_err());
        assert_eq!(load(&dir.0).unwrap().phase, Phase::Blocked);
        assert!(!env.gate_open.get());

        // Simulate crash mid-Activating AFTER switch completed -> completes recovery & reopens gate!
        st.phase = Phase::Activating;
        env.switched.set(true);
        save(&dir.0, &st).unwrap();
        recover_locked(&dir.0, &policy, &env).unwrap();
        assert_eq!(load(&dir.0).unwrap().phase, Phase::Activated);
        assert!(env.gate_open.get());
    }

    #[test]
    fn test_lock_excludes_concurrent_operations() {
        let dir = TempDir::new();
        let held = lock(&dir.0).unwrap();
        assert!(lock(&dir.0).is_err());
        drop(held);
        assert!(lock(&dir.0).is_ok());
    }
}
