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
    granted_at: Instant,
}

struct Inner {
    dir: PathBuf,
    gate: Gate,
    next_seq: u64,
    in_flight_starts: usize,
    pending_admissions: BTreeMap<u64, (String, String)>,
    tracked_runs: BTreeSet<TrackedKey>,
    leases: HashMap<Vec<u8>, Lease>,
}

pub struct Coordinator {
    inner: Mutex<Inner>,
    starts_drained: Notify,
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
    pub fn open(dir: &Path) -> Result<Self, Error> {
        match DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::Storage(format!("create {}: {e}", dir.display()))),
        }
        let gate = load_gate(dir)?;
        let (pending, mut tracked) = load_ledger(dir)?;
        // Any pending admission left over from an interrupted broker process is
        // promoted to a tracked lookup so drain reconciles it against History.
        for (namespace, workflow_id) in pending.values() {
            tracked.insert(TrackedKey {
                namespace: namespace.clone(),
                workflow_id: workflow_id.clone(),
                run_id: None,
            });
        }
        let mut inner = Inner {
            dir: dir.to_path_buf(),
            gate,
            next_seq: 1,
            in_flight_starts: 0,
            pending_admissions: BTreeMap::new(),
            tracked_runs: tracked,
            leases: HashMap::new(),
        };
        if !pending.is_empty() {
            save_ledger(&inner)?;
        } else {
            let _ = &mut inner;
        }
        Ok(Self {
            inner: Mutex::new(inner),
            starts_drained: Notify::new(),
        })
    }

    pub fn gate_snapshot(&self) -> Gate {
        self.inner.lock().unwrap().gate.clone()
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
                guard.gate = Gate::Open;
                save_gate(&guard.dir, &guard.gate)
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
                granted_at: Instant::now(),
            },
        );
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
    atomic_write(&inner.dir, "ledger.state", &out)
}

type LoadedLedger = (BTreeMap<u64, (String, String)>, BTreeSet<TrackedKey>);

fn load_ledger(dir: &Path) -> Result<LoadedLedger, Error> {
    let path = dir.join("ledger.state");
    let text = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((BTreeMap::new(), BTreeSet::new()));
        }
        Err(e) => return Err(Error::Storage(format!("read {}: {e}", path.display()))),
    };
    let mut lines = text.lines();
    if lines.next() != Some("version=1") {
        return Err(Error::Storage("unsupported ledger.state version".into()));
    }
    let mut pending = BTreeMap::new();
    let mut runs = BTreeSet::new();
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
        } else {
            return Err(Error::Storage("unexpected line in ledger.state".into()));
        }
    }
    Ok((pending, runs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
        let coord = Arc::new(Coordinator::open(&dir).unwrap());
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
        let recovered = Coordinator::open(&dir).unwrap();
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
