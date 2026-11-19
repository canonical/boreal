use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

pub const DEFAULT_STATE_DIR: &str = "/var/lib/boreal-broker";
const LEASE_TTL: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    Open,
    Closed {
        update_id: String,
        rev: String,
        paused_schedules: Vec<(String, String)>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackedKey {
    pub namespace: String,
    pub workflow_id: String,
    pub run_id: Option<String>,
}

#[derive(Clone, Debug)]
struct Lease {
    namespace: String,
    granted_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RemoteTaskKind {
    Workflow,
    Activity,
    Query,
}

impl RemoteTaskKind {
    fn label(self) -> &'static str {
        match self {
            Self::Workflow => "workflow",
            Self::Activity => "activity",
            Self::Query => "query",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RemoteLease {
    token_digest: String,
    namespace: String,
    task_queue: String,
    kind: RemoteTaskKind,
}

impl RemoteLease {
    fn new(token: &[u8], namespace: &str, task_queue: &str, kind: RemoteTaskKind) -> Self {
        Self {
            token_digest: digest_hex(token),
            namespace: namespace.to_owned(),
            task_queue: task_queue.to_owned(),
            kind,
        }
    }
}

pub(crate) fn digest_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in ring::digest::digest(&ring::digest::SHA256, data).as_ref() {
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    hex
}

struct Inner {
    dir: PathBuf,
    gate: Gate,
    next_seq: u64,
    in_flight_starts: usize,
    pending_admissions: BTreeMap<u64, (String, String)>,
    tracked_runs: BTreeSet<TrackedKey>,
    leases: HashMap<Vec<u8>, Lease>,
    remote_leases: BTreeSet<RemoteLease>,
    remote_fenced: bool,
    remote_pickups: usize,
}

pub struct Coordinator {
    inner: Mutex<Inner>,
    starts_drained: Notify,
    remote_enabled: bool,
}

pub struct RemotePickup<'a>(&'a Coordinator);

impl Drop for RemotePickup<'_> {
    fn drop(&mut self) {
        self.0.inner.lock().unwrap().remote_pickups -= 1;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RemoteDrain {
    pub fenced: bool,
    pub pickups: usize,
    pub leases: usize,
}

#[derive(Debug)]
pub enum Error {
    AdmissionClosed,
    Conflict(String),
    Storage(String),
}

impl Error {
    pub fn detail(&self) -> String {
        match self {
            Self::AdmissionClosed => {
                "system update in progress: new workflow admissions are temporarily closed".into()
            }
            Self::Conflict(msg) | Self::Storage(msg) => msg.clone(),
        }
    }
}

impl Coordinator {
    pub fn open(dir: &Path, remote_enabled: bool) -> Result<Self, Error> {
        match DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::Storage(format!("create {}: {e}", dir.display()))),
        }
        let gate = load_gate(dir)?;
        let (pending, mut tracked, remote_leases) = load_ledger(dir)?;
        // Any pending admission left over from an interrupted broker process is
        // promoted to a tracked lookup so drain reconciles it against History.
        for (namespace, workflow_id) in pending.values() {
            tracked.insert(TrackedKey {
                namespace: namespace.clone(),
                workflow_id: workflow_id.clone(),
                run_id: None,
            });
        }
        let inner = Inner {
            dir: dir.to_path_buf(),
            // A restarted broker cannot assume the updater hasn't already
            // observed quiescence. Never allow pickup on a recovered closed gate.
            remote_fenced: matches!(gate, Gate::Closed { .. }),
            gate,
            next_seq: 1,
            in_flight_starts: 0,
            pending_admissions: BTreeMap::new(),
            tracked_runs: tracked,
            leases: HashMap::new(),
            remote_leases,
            remote_pickups: 0,
        };
        if !pending.is_empty() {
            save_ledger(&inner)?;
        }
        Ok(Self {
            inner: Mutex::new(inner),
            starts_drained: Notify::new(),
            remote_enabled,
        })
    }

    pub fn gate_snapshot(&self) -> Gate {
        self.inner.lock().unwrap().gate.clone()
    }

    pub fn begin_remote_pickup(&self) -> Result<RemotePickup<'_>, Error> {
        let mut guard = self.inner.lock().unwrap();
        if !self.remote_enabled || guard.remote_fenced {
            return Err(Error::AdmissionClosed);
        }
        guard.remote_pickups += 1;
        Ok(RemotePickup(self))
    }

    pub fn remote_drain(&self, update_id: &str, fence: bool) -> Result<RemoteDrain, Error> {
        let mut guard = self.inner.lock().unwrap();
        if !matches!(&guard.gate, Gate::Closed { update_id: id, .. } if id == update_id)
            || guard.in_flight_starts != 0
        {
            return Err(Error::Conflict(
                "admission gate is not closed for this update".into(),
            ));
        }
        if fence && self.remote_enabled {
            guard.remote_fenced = true;
        }
        Ok(RemoteDrain {
            fenced: guard.remote_fenced || !self.remote_enabled,
            pickups: guard.remote_pickups,
            leases: guard.remote_leases.len(),
        })
    }

    pub fn begin_start(&self, namespace: &str, workflow_id: &str) -> Result<u64, Error> {
        let mut guard = self.inner.lock().unwrap();
        if !matches!(guard.gate, Gate::Open) {
            return Err(Error::AdmissionClosed);
        }
        let seq = guard.next_seq;
        guard.next_seq += 1;
        guard
            .pending_admissions
            .insert(seq, (namespace.to_owned(), workflow_id.to_owned()));
        if let Err(err) = save_ledger(&guard) {
            guard.pending_admissions.remove(&seq);
            return Err(err);
        }
        guard.in_flight_starts += 1;
        Ok(seq)
    }

    pub fn finish_start(&self, seq: u64, run_id: Option<String>) -> Result<(), Error> {
        let notify = {
            let mut guard = self.inner.lock().unwrap();
            if let Some((namespace, workflow_id)) = guard.pending_admissions.remove(&seq) {
                guard.tracked_runs.insert(TrackedKey {
                    namespace,
                    workflow_id,
                    run_id,
                });
            }
            let save_res = save_ledger(&guard);
            guard.in_flight_starts = guard.in_flight_starts.saturating_sub(1);
            let done = guard.in_flight_starts == 0;
            save_res?;
            done
        };
        if notify {
            self.starts_drained.notify_waiters();
        }
        Ok(())
    }

    pub async fn close_gate(
        &self,
        update_id: &str,
        rev: &str,
    ) -> Result<Vec<(String, String)>, Error> {
        loop {
            let notified = self.starts_drained.notified();
            {
                let mut guard = self.inner.lock().unwrap();
                let paused = match &guard.gate {
                    Gate::Open => Vec::new(),
                    Gate::Closed {
                        update_id: existing,
                        paused_schedules,
                        ..
                    } => {
                        if existing != update_id {
                            return Err(Error::Conflict(format!(
                                "admission gate is already closed by update '{existing}'"
                            )));
                        }
                        paused_schedules.clone()
                    }
                };
                if matches!(guard.gate, Gate::Open) {
                    guard.gate = Gate::Closed {
                        update_id: update_id.to_owned(),
                        rev: rev.to_owned(),
                        paused_schedules: paused.clone(),
                    };
                    save_gate(&guard.dir, &guard.gate)?;
                }
                if guard.in_flight_starts == 0 {
                    return Ok(paused);
                }
            }
            notified.await;
        }
    }

    pub fn add_paused_schedules(
        &self,
        update_id: &str,
        schedules: &[(String, String)],
    ) -> Result<Vec<(String, String)>, Error> {
        let mut guard = self.inner.lock().unwrap();
        let dir = guard.dir.clone();
        match &mut guard.gate {
            Gate::Closed {
                update_id: existing,
                paused_schedules,
                ..
            } if existing == update_id => {
                for item in schedules {
                    if !paused_schedules.contains(item) {
                        paused_schedules.push(item.clone());
                    }
                }
                let snapshot = paused_schedules.clone();
                save_gate(&dir, &guard.gate)?;
                Ok(snapshot)
            }
            _ => Err(Error::Conflict(
                "admission gate is not closed for this update".into(),
            )),
        }
    }

    pub fn require_closed_for(&self, update_id: &str) -> Result<Vec<(String, String)>, Error> {
        let guard = self.inner.lock().unwrap();
        match &guard.gate {
            Gate::Closed {
                update_id: existing,
                paused_schedules,
                ..
            } if existing == update_id && guard.in_flight_starts == 0 => {
                Ok(paused_schedules.clone())
            }
            Gate::Closed {
                update_id: existing,
                ..
            } if existing != update_id => Err(Error::Conflict(format!(
                "gate is owned by update '{existing}', not '{update_id}'"
            ))),
            _ => Err(Error::Conflict(
                "admission gate is not closed or in-flight starts remain".into(),
            )),
        }
    }

    pub fn reopen_gate(&self, update_id: &str) -> Result<(), Error> {
        let mut guard = self.inner.lock().unwrap();
        match &guard.gate {
            Gate::Open => Ok(()),
            Gate::Closed {
                update_id: existing,
                ..
            } if existing == update_id => {
                save_gate(&guard.dir, &Gate::Open)?;
                guard.gate = Gate::Open;
                guard.remote_fenced = false;
                Ok(())
            }
            Gate::Closed {
                update_id: existing,
                ..
            } => Err(Error::Conflict(format!(
                "cannot reopen gate owned by '{existing}'"
            ))),
        }
    }

    pub fn track_runs(&self, keys: impl IntoIterator<Item = TrackedKey>) -> Result<(), Error> {
        let mut guard = self.inner.lock().unwrap();
        let mut changed = false;
        for key in keys {
            if !key.workflow_id.is_empty() && guard.tracked_runs.insert(key) {
                changed = true;
            }
        }
        if changed {
            save_ledger(&guard)?;
        }
        Ok(())
    }

    pub fn tracked_runs_snapshot(&self) -> Vec<TrackedKey> {
        self.inner
            .lock()
            .unwrap()
            .tracked_runs
            .iter()
            .cloned()
            .collect()
    }

    pub fn retire_runs(&self, retired: &[TrackedKey]) -> Result<(), Error> {
        if retired.is_empty() {
            return Ok(());
        }
        let mut guard = self.inner.lock().unwrap();
        let mut changed = false;
        for key in retired {
            if guard.tracked_runs.remove(key) {
                changed = true;
            }
        }
        if changed {
            save_ledger(&guard)?;
        }
        Ok(())
    }

    pub fn record_lease(
        &self,
        task_token: Vec<u8>,
        namespace: &str,
        workflow_id: &str,
        run_id: &str,
    ) {
        if task_token.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        if !workflow_id.is_empty() {
            let inserted = guard.tracked_runs.insert(TrackedKey {
                namespace: namespace.to_owned(),
                workflow_id: workflow_id.to_owned(),
                run_id: if run_id.is_empty() {
                    None
                } else {
                    Some(run_id.to_owned())
                },
            });
            if inserted {
                let _ = save_ledger(&guard);
            }
        }
        guard.leases.insert(
            task_token,
            Lease {
                namespace: namespace.to_owned(),
                granted_at: Instant::now(),
            },
        );
    }

    /// Persist before giving a remote worker the reply. Unlike local work,
    /// remote execution is not stopped by shutting down the local units or
    /// restarting the broker. A timer alone is not proof it stopped.
    pub fn record_remote_lease(
        &self,
        token: &[u8],
        kind: RemoteTaskKind,
        task_queue: &str,
        key: TrackedKey,
    ) -> Result<(), Error> {
        let lease = RemoteLease::new(token, &key.namespace, task_queue, kind);
        let mut guard = self.inner.lock().unwrap();
        if !guard.remote_leases.insert(lease.clone()) {
            return Err(Error::Conflict(
                "remote task has an outstanding lease".into(),
            ));
        }
        let inserted = guard.tracked_runs.insert(key.clone());
        if let Err(err) = save_ledger(&guard) {
            guard.remote_leases.remove(&lease);
            if inserted {
                guard.tracked_runs.remove(&key);
            }
            return Err(err);
        }
        Ok(())
    }

    pub fn has_remote_lease(
        &self,
        token: &[u8],
        namespace: &str,
        task_queue: &str,
        kind: RemoteTaskKind,
    ) -> bool {
        self.inner
            .lock()
            .unwrap()
            .remote_leases
            .contains(&RemoteLease::new(token, namespace, task_queue, kind))
    }

    pub fn finish_remote_lease(
        &self,
        token: &[u8],
        namespace: &str,
        task_queue: &str,
        kind: RemoteTaskKind,
    ) -> Result<(), Error> {
        let lease = RemoteLease::new(token, namespace, task_queue, kind);
        let mut guard = self.inner.lock().unwrap();
        if guard.remote_leases.remove(&lease)
            && let Err(err) = save_ledger(&guard)
        {
            guard.remote_leases.insert(lease);
            return Err(err);
        }
        Ok(())
    }

    /// True when this namespace still has a drain-tracked run or a live worker lease.
    ///
    /// A closed admission gate uses this so an in-flight caller can start a delegated
    /// workflow without letting idle callers admit new work.
    pub fn namespace_has_inflight(&self, namespace: &str) -> bool {
        let mut guard = self.inner.lock().unwrap();
        let now = Instant::now();
        guard
            .leases
            .retain(|_, lease| now.duration_since(lease.granted_at) < LEASE_TTL);
        guard
            .leases
            .values()
            .any(|lease| lease.namespace == namespace)
            || guard
                .remote_leases
                .iter()
                .any(|lease| lease.namespace == namespace)
            || guard
                .tracked_runs
                .iter()
                .any(|key| key.namespace == namespace)
    }

    pub fn complete_lease(&self, task_token: &[u8]) {
        if task_token.is_empty() {
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        guard.leases.remove(task_token);
    }

    pub fn active_lease_count(&self) -> usize {
        let mut guard = self.inner.lock().unwrap();
        let now = Instant::now();
        guard
            .leases
            .retain(|_, lease| now.duration_since(lease.granted_at) < LEASE_TTL);
        guard.leases.len()
    }
}

fn sanitize_field(s: &str) -> Result<(), Error> {
    if s.contains(['\n', '\r', '\t', '\0']) {
        return Err(Error::Storage(
            "control characters forbidden in persisted ledger field".into(),
        ));
    }
    Ok(())
}

fn atomic_write(dir: &Path, name: &str, content: &str) -> Result<(), Error> {
    let tmp = dir.join(format!("{name}.tmp"));
    let dst = dir.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| Error::Storage(format!("open {}: {e}", tmp.display())))?;
    file.write_all(content.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| Error::Storage(format!("write {}: {e}", tmp.display())))?;
    fs::rename(&tmp, &dst).map_err(|e| Error::Storage(format!("rename {}: {e}", dst.display())))?;
    File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| Error::Storage(format!("sync {}: {e}", dir.display())))
}

fn save_gate(dir: &Path, gate: &Gate) -> Result<(), Error> {
    let mut out = String::from("version=1\n");
    match gate {
        Gate::Open => out.push_str("mode=open\n"),
        Gate::Closed {
            update_id,
            rev,
            paused_schedules,
        } => {
            sanitize_field(update_id)?;
            sanitize_field(rev)?;
            out.push_str("mode=closed\n");
            out.push_str(&format!("update_id={update_id}\nrev={rev}\n"));
            for (ns, sched) in paused_schedules {
                sanitize_field(ns)?;
                sanitize_field(sched)?;
                out.push_str(&format!("paused={ns}\t{sched}\n"));
            }
        }
    }
    atomic_write(dir, "gate.state", &out)
}

fn load_gate(dir: &Path) -> Result<Gate, Error> {
    let path = dir.join("gate.state");
    let text = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Gate::Open),
        Err(e) => return Err(Error::Storage(format!("read {}: {e}", path.display()))),
    };
    let mut lines = text.lines();
    if lines.next() != Some("version=1") {
        return Err(Error::Storage("unsupported gate.state version".into()));
    }
    match lines.next() {
        Some("mode=open") => Ok(Gate::Open),
        Some("mode=closed") => {
            let update_id = lines
                .next()
                .and_then(|l| l.strip_prefix("update_id="))
                .ok_or_else(|| Error::Storage("missing update_id in gate.state".into()))?
                .to_owned();
            let rev = lines
                .next()
                .and_then(|l| l.strip_prefix("rev="))
                .ok_or_else(|| Error::Storage("missing rev in gate.state".into()))?
                .to_owned();
            let mut paused_schedules = Vec::new();
            for line in lines {
                let rest = line
                    .strip_prefix("paused=")
                    .ok_or_else(|| Error::Storage("invalid line in gate.state".into()))?;
                let (ns, sched) = rest
                    .split_once('\t')
                    .ok_or_else(|| Error::Storage("invalid paused schedule record".into()))?;
                paused_schedules.push((ns.to_owned(), sched.to_owned()));
            }
            Ok(Gate::Closed {
                update_id,
                rev,
                paused_schedules,
            })
        }
        _ => Err(Error::Storage("invalid mode in gate.state".into())),
    }
}

fn save_ledger(inner: &Inner) -> Result<(), Error> {
    let mut out = String::from("version=1\n");
    for (seq, (ns, wf)) in &inner.pending_admissions {
        sanitize_field(ns)?;
        sanitize_field(wf)?;
        out.push_str(&format!("pending={seq}\t{ns}\t{wf}\n"));
    }
    for key in &inner.tracked_runs {
        sanitize_field(&key.namespace)?;
        sanitize_field(&key.workflow_id)?;
        let run = key.run_id.as_deref().unwrap_or("");
        sanitize_field(run)?;
        out.push_str(&format!(
            "run={}\t{}\t{run}\n",
            key.namespace, key.workflow_id
        ));
    }
    for lease in &inner.remote_leases {
        sanitize_field(&lease.namespace)?;
        sanitize_field(&lease.task_queue)?;
        out.push_str(&format!(
            "remote={}\t{}\t{}\t{}\n",
            lease.token_digest,
            lease.namespace,
            lease.task_queue,
            lease.kind.label()
        ));
    }
    atomic_write(&inner.dir, "ledger.state", &out)
}

type LoadedLedger = (
    BTreeMap<u64, (String, String)>,
    BTreeSet<TrackedKey>,
    BTreeSet<RemoteLease>,
);

fn load_ledger(dir: &Path) -> Result<LoadedLedger, Error> {
    let path = dir.join("ledger.state");
    let text = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((BTreeMap::new(), BTreeSet::new(), BTreeSet::new()));
        }
        Err(e) => return Err(Error::Storage(format!("read {}: {e}", path.display()))),
    };
    let mut lines = text.lines();
    if lines.next() != Some("version=1") {
        return Err(Error::Storage("unsupported ledger.state version".into()));
    }
    let mut pending = BTreeMap::new();
    let mut runs = BTreeSet::new();
    let mut remote_leases = BTreeSet::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix("pending=") {
            let mut parts = rest.splitn(3, '\t');
            let seq: u64 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Error::Storage("invalid pending seq".into()))?;
            let ns = parts
                .next()
                .ok_or_else(|| Error::Storage("missing pending ns".into()))?;
            let wf = parts
                .next()
                .ok_or_else(|| Error::Storage("missing pending wf".into()))?;
            pending.insert(seq, (ns.to_owned(), wf.to_owned()));
        } else if let Some(rest) = line.strip_prefix("run=") {
            let mut parts = rest.splitn(3, '\t');
            let ns = parts
                .next()
                .ok_or_else(|| Error::Storage("missing run ns".into()))?;
            let wf = parts
                .next()
                .ok_or_else(|| Error::Storage("missing run wf".into()))?;
            let rid = parts
                .next()
                .ok_or_else(|| Error::Storage("missing run id".into()))?;
            runs.insert(TrackedKey {
                namespace: ns.to_owned(),
                workflow_id: wf.to_owned(),
                run_id: if rid.is_empty() {
                    None
                } else {
                    Some(rid.to_owned())
                },
            });
        } else if let Some(rest) = line.strip_prefix("remote=") {
            let mut fields = rest.split('\t');
            let invalid = || Error::Storage("invalid remote lease record in ledger.state".into());
            let token_digest = fields
                .next()
                .filter(|s| {
                    s.len() == 64
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
                .ok_or_else(invalid)?;
            let namespace = fields
                .next()
                .filter(|s| !s.is_empty())
                .ok_or_else(invalid)?;
            let task_queue = fields
                .next()
                .filter(|s| !s.is_empty())
                .ok_or_else(invalid)?;
            let kind = match fields.next() {
                Some("workflow") => RemoteTaskKind::Workflow,
                Some("activity") => RemoteTaskKind::Activity,
                Some("query") => RemoteTaskKind::Query,
                _ => return Err(invalid()),
            };
            if fields.next().is_some()
                || !remote_leases.insert(RemoteLease {
                    token_digest: token_digest.to_owned(),
                    namespace: namespace.to_owned(),
                    task_queue: task_queue.to_owned(),
                    kind,
                })
            {
                return Err(invalid());
            }
        } else {
            return Err(Error::Storage("unexpected line in ledger.state".into()));
        }
    }
    Ok((pending, runs, remote_leases))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn recovered_closed_gate_and_remote_leases_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let token = b"sample task token";
        let kind = RemoteTaskKind::Workflow;
        let coord = Coordinator::open(dir.path(), true).unwrap();
        coord
            .record_remote_lease(
                token,
                kind,
                "alice-greet",
                TrackedKey {
                    namespace: "alice".into(),
                    workflow_id: "test".into(),
                    run_id: Some("run".into()),
                },
            )
            .unwrap();
        coord.close_gate("update", "rev").await.unwrap();
        assert!(coord.begin_remote_pickup().is_ok());
        drop(coord);

        // Also preserve leases if a replacement deployment removes ingress.
        let no_ingress = Coordinator::open(dir.path(), false).unwrap();
        assert_eq!(
            no_ingress.remote_drain("update", false).unwrap(),
            RemoteDrain {
                fenced: true,
                pickups: 0,
                leases: 1,
            }
        );
        drop(no_ingress);
        let coord = Coordinator::open(dir.path(), true).unwrap();
        assert!(coord.begin_remote_pickup().is_err());
        assert!(coord.has_remote_lease(token, "alice", "alice-greet", kind));
        assert!(!coord.has_remote_lease(token, "alice", "alice-wrong", kind));
        assert!(!coord.has_remote_lease(token, "alice", "alice-greet", RemoteTaskKind::Query));
        assert!(
            !fs::read_to_string(dir.path().join("ledger.state"))
                .unwrap()
                .contains("sample task token")
        );
        coord
            .finish_remote_lease(token, "alice", "alice-greet", kind)
            .unwrap();
        assert_eq!(coord.remote_drain("update", false).unwrap().leases, 0);
        assert!(coord.reopen_gate("wrong-update").is_err());
        assert!(coord.begin_remote_pickup().is_err());
        coord.reopen_gate("update").unwrap();
        assert!(coord.begin_remote_pickup().is_ok());
        drop(coord);
        let coord = Coordinator::open(dir.path(), true).unwrap();
        assert!(coord.begin_remote_pickup().is_ok());
        assert!(!coord.has_remote_lease(token, "alice", "alice-greet", kind));
    }

    #[tokio::test]
    async fn test_durable_gate_waits_for_inflight_starts_and_survives_restart() {
        let dir = std::env::temp_dir().join(format!(
            "boreal-ledger-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let coord = Arc::new(Coordinator::open(&dir, false).unwrap());
        let seq = coord.begin_start("alice", "wf-1").unwrap();

        let closer = {
            let coord = Arc::clone(&coord);
            tokio::spawn(async move { coord.close_gate("upd-1", "abcd").await.unwrap() })
        };
        tokio::task::yield_now().await;
        assert!(matches!(
            coord.begin_start("alice", "wf-2"),
            Err(Error::AdmissionClosed)
        ));
        assert!(!closer.is_finished());

        coord.finish_start(seq, Some("run-1".into())).unwrap();
        closer.await.unwrap();
        coord
            .add_paused_schedules("upd-1", &[("alice".into(), "sched-1".into())])
            .unwrap();

        drop(coord);
        let recovered = Coordinator::open(&dir, false).unwrap();
        assert_eq!(
            recovered.gate_snapshot(),
            Gate::Closed {
                update_id: "upd-1".into(),
                rev: "abcd".into(),
                paused_schedules: vec![("alice".into(), "sched-1".into())],
            }
        );
        assert!(matches!(
            recovered.begin_start("alice", "wf-3"),
            Err(Error::AdmissionClosed)
        ));
        assert_eq!(recovered.tracked_runs_snapshot().len(), 1);
        recovered.reopen_gate("upd-1").unwrap();
        assert_eq!(recovered.gate_snapshot(), Gate::Open);

        let _ = fs::remove_dir_all(&dir);
    }
}
