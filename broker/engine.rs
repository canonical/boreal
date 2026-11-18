use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use boreal_common::catalog::{self, Catalog};
use boreal_common::wire::{
    ErrorCode, PromptSummary, Request, Response, ResultOutcome, RunInput, UpdaterRequest,
    UpdaterResponse, WorkflowCounts, WorkflowDetail, WorkflowSummary, WorkflowView,
};

use crate::ledger::{self, Coordinator, Gate, TrackedKey};
use crate::pg;
use crate::temporal::{Backend, BackendError, CloseOutcome, RawExecution};

pub struct Engine<B: Backend> {
    catalog: Catalog,
    coordinator: Arc<Coordinator>,
    backend: Arc<B>,
}

impl<B: Backend> Engine<B> {
    pub fn new(catalog: Catalog, coordinator: Arc<Coordinator>, backend: Arc<B>) -> Self {
        Self {
            catalog,
            coordinator,
            backend,
        }
    }

    pub async fn handle_user_request(&self, caller: &str, req: Request) -> Response {
        if !self.catalog.is_user(caller) {
            return Response::Error {
                code: ErrorCode::PermissionDenied,
                message: format!("OS account '{caller}' is not a registered Boreal user"),
            };
        }
        match self.dispatch_user(caller, req).await {
            Ok(resp) => resp,
            Err((code, message)) => Response::Error { code, message },
        }
    }

    async fn dispatch_user(
        &self,
        caller: &str,
        req: Request,
    ) -> Result<Response, (ErrorCode, String)> {
        match req {
            Request::Workers => {
                let entries = self
                    .catalog
                    .list_accessible_workers(caller)
                    .map_err(map_catalog_err)?;
                Ok(Response::Workers(entries))
            }
            Request::Permissions { user } => {
                let report = self
                    .catalog
                    .inspect_permissions(caller, user.as_deref())
                    .map_err(map_catalog_err)?;
                Ok(Response::Permissions(report))
            }
            Request::Run {
                owner,
                worker,
                workflow,
                id,
                input,
            } => {
                catalog::validate_workflow_id(&id).map_err(map_catalog_err)?;
                let (w, pkg) = self
                    .catalog
                    .worker_and_package(&owner, &worker)
                    .map_err(map_catalog_err)?;
                if !self.catalog.can_write(caller, &owner, w.name()) {
                    return Err((
                        ErrorCode::PermissionDenied,
                        format!(
                            "permission denied: '{caller}' lacks write access to '{owner}/{worker}'"
                        ),
                    ));
                }
                let workflow = pkg.select(workflow.as_deref()).map_err(map_catalog_err)?;
                let encoded = match input {
                    RunInput::Positional(args) => workflow.validate_and_encode(&args),
                    RunInput::Json(raw) => workflow.validate_json(&raw),
                }
                .map_err(map_catalog_err)?;
                let task_queue = format!("{owner}-{}", w.name());
                let seq = self
                    .coordinator
                    .begin_start(&owner, &id)
                    .map_err(map_ledger_err)?;
                let start_res = self
                    .backend
                    .start_workflow(&owner, &task_queue, workflow.workflow_type(), &id, &encoded)
                    .await;
                match start_res {
                    Ok(run_id) => {
                        self.coordinator
                            .finish_start(seq, Some(run_id.clone()))
                            .map_err(map_ledger_err)?;
                        Ok(Response::Started {
                            owner,
                            worker: w.name().to_owned(),
                            workflow_type: workflow.workflow_type().to_owned(),
                            workflow_id: id,
                            run_id,
                        })
                    }
                    Err(err) => {
                        let _ = self.coordinator.finish_start(seq, None);
                        Err(map_backend_err(err))
                    }
                }
            }
            Request::List { owner } => {
                self.catalog.require_user(&owner).map_err(map_catalog_err)?;
                if !self.catalog.has_any_read(caller, &owner) {
                    return Err((
                        ErrorCode::PermissionDenied,
                        format!(
                            "permission denied: '{caller}' lacks read access to '{owner}' workflows"
                        ),
                    ));
                }
                let raw_list = self
                    .backend
                    .list_workflows(&owner)
                    .await
                    .map_err(map_backend_err)?;
                let mut visible = Vec::new();
                for exec in raw_list {
                    let Ok(verified) = self.catalog.verify_execution(
                        &owner,
                        &exec.task_queue,
                        &exec.workflow_type,
                        &exec.run_id,
                    ) else {
                        continue;
                    };
                    if !self
                        .catalog
                        .can_read(caller, &owner, verified.worker.name())
                    {
                        continue;
                    }
                    visible.push(WorkflowSummary {
                        owner: owner.clone(),
                        worker: verified.worker.name().to_owned(),
                        workflow_type: verified.workflow.workflow_type().to_owned(),
                        workflow_id: exec.workflow_id,
                        run_id: exec.run_id,
                        status: exec.status.label().to_owned(),
                    });
                }
                Ok(Response::List(visible))
            }
            Request::Status { owner, id } => {
                catalog::validate_workflow_id(&id).map_err(map_catalog_err)?;
                let detail = self
                    .authorize_and_describe_read(caller, &owner, &id)
                    .await?;
                Ok(Response::Status(detail))
            }
            Request::Result { owner, id } => {
                catalog::validate_workflow_id(&id).map_err(map_catalog_err)?;
                let detail = self
                    .authorize_and_describe_read(caller, &owner, &id)
                    .await?;
                if detail.status == "running" {
                    return Ok(Response::WorkflowResult(ResultOutcome::Running {
                        run_id: detail.run_id,
                    }));
                }
                let close = self
                    .backend
                    .fetch_close_outcome(&owner, &id, &detail.run_id)
                    .await
                    .map_err(map_backend_err)?;
                let outcome = match close {
                    CloseOutcome::Completed(output) => ResultOutcome::Completed {
                        run_id: detail.run_id,
                        output,
                    },
                    CloseOutcome::Running => ResultOutcome::Running {
                        run_id: detail.run_id,
                    },
                    CloseOutcome::Unsuccessful {
                        status,
                        detail: msg,
                    } => ResultOutcome::Unsuccessful {
                        run_id: detail.run_id,
                        status,
                        detail: msg,
                    },
                };
                Ok(Response::WorkflowResult(outcome))
            }
            Request::PrepareCancel { owner, id } => {
                catalog::validate_workflow_id(&id).map_err(map_catalog_err)?;
                let detail = self
                    .authorize_and_describe_write(caller, &owner, &id, None)
                    .await?;
                Ok(Response::CancelPrepared(detail))
            }
            Request::Cancel { owner, id, run_id } => {
                catalog::validate_workflow_id(&id).map_err(map_catalog_err)?;
                if run_id.trim().is_empty() {
                    return Err((
                        ErrorCode::InvalidArgument,
                        "cancel requires a verified run ID".into(),
                    ));
                }
                let detail = self
                    .authorize_and_describe_write(caller, &owner, &id, Some(&run_id))
                    .await?;
                self.backend
                    .cancel_workflow(&owner, &id, &detail.run_id)
                    .await
                    .map_err(map_backend_err)?;
                Ok(Response::Cancelled {
                    owner,
                    worker: detail.worker,
                    workflow_type: detail.workflow_type,
                    workflow_id: id,
                    run_id: detail.run_id,
                })
            }
            Request::Summary => {
                let draining = !matches!(self.coordinator.gate_snapshot(), Gate::Open);
                let has_own_workers = self
                    .catalog
                    .users()
                    .get(caller)
                    .is_some_and(|policy| !policy.workers().is_empty());
                let workflows = if !has_own_workers {
                    Some(WorkflowCounts::default())
                } else {
                    match tokio::time::timeout(
                        Duration::from_millis(200),
                        self.backend.count_open_workflows(caller),
                    )
                    .await
                    {
                        Ok(Ok(counts)) => Some(counts),
                        Ok(Err(_)) | Err(_) => None,
                    }
                };
                Ok(Response::Summary(PromptSummary {
                    workflows,
                    draining,
                }))
            }
            Request::View { owner, id } => {
                catalog::validate_workflow_id(&id).map_err(map_catalog_err)?;
                let detail = self
                    .authorize_and_describe_read(caller, &owner, &id)
                    .await?;
                let insp = self
                    .backend
                    .inspect_workflow(&owner, &id, &detail.run_id)
                    .await
                    .map_err(map_backend_err)?;
                let steps = insp
                    .steps
                    .into_iter()
                    .map(|mut item| {
                        if let Some((ns, tq)) = item.child_queue
                            && !self.can_read_child_queue(caller, &owner, &ns, &tq)
                            && !item.step.detail.is_empty()
                        {
                            item.step.detail = "[redacted]".into();
                        }
                        item.step
                    })
                    .collect();
                let events = insp
                    .events
                    .into_iter()
                    .map(|mut item| {
                        if let Some((ns, tq)) = item.child_queue
                            && !self.can_read_child_queue(caller, &owner, &ns, &tq)
                        {
                            item.event.summary = "[redacted]".into();
                        }
                        item.event
                    })
                    .collect();
                let outcome = match insp.outcome {
                    Some(CloseOutcome::Completed(output)) => Some(ResultOutcome::Completed {
                        run_id: detail.run_id.clone(),
                        output,
                    }),
                    Some(CloseOutcome::Unsuccessful {
                        status,
                        detail: msg,
                    }) => Some(ResultOutcome::Unsuccessful {
                        run_id: detail.run_id.clone(),
                        status,
                        detail: msg,
                    }),
                    Some(CloseOutcome::Running) | None => None,
                };
                Ok(Response::View(WorkflowView {
                    owner: detail.owner,
                    worker: detail.worker,
                    workflow_type: detail.workflow_type,
                    workflow_id: detail.workflow_id,
                    run_id: detail.run_id,
                    status: detail.status,
                    start_time: detail.start_time,
                    close_time: detail.close_time,
                    elapsed: insp.elapsed,
                    input: insp.input,
                    task_warning: insp.task_warning,
                    steps,
                    events,
                    outcome,
                }))
            }
        }
    }

    fn can_read_child_queue(&self, caller: &str, owner: &str, ns: &str, task_queue: &str) -> bool {
        if ns != owner {
            return false;
        }
        let prefix = format!("{owner}-");
        let Some(worker) = task_queue.strip_prefix(&prefix) else {
            return false;
        };
        self.catalog.can_read(caller, owner, worker)
    }

    async fn authorize_and_describe_read(
        &self,
        caller: &str,
        owner: &str,
        id: &str,
    ) -> Result<WorkflowDetail, (ErrorCode, String)> {
        self.catalog.require_user(owner).map_err(map_catalog_err)?;
        if !self.catalog.has_any_read(caller, owner) {
            return Err((
                ErrorCode::PermissionDenied,
                format!("permission denied: '{caller}' lacks read access to '{owner}' workflows"),
            ));
        }
        let exec = self
            .backend
            .describe_workflow(owner, id, None)
            .await
            .map_err(map_backend_err)?;
        let verified = self
            .catalog
            .verify_execution(owner, &exec.task_queue, &exec.workflow_type, &exec.run_id)
            .map_err(|e| (ErrorCode::PermissionDenied, e.detail()))?;
        if !self.catalog.can_read(caller, owner, verified.worker.name()) {
            return Err((
                ErrorCode::PermissionDenied,
                format!(
                    "permission denied: '{caller}' lacks read access to '{owner}/{}'",
                    verified.worker.name()
                ),
            ));
        }
        Ok(to_detail(
            owner,
            verified.worker.name(),
            verified.workflow.workflow_type(),
            exec,
        ))
    }

    async fn authorize_and_describe_write(
        &self,
        caller: &str,
        owner: &str,
        id: &str,
        pinned_run_id: Option<&str>,
    ) -> Result<WorkflowDetail, (ErrorCode, String)> {
        self.catalog.require_user(owner).map_err(map_catalog_err)?;
        if !self.catalog.has_any_write(caller, owner) {
            return Err((
                ErrorCode::PermissionDenied,
                format!("permission denied: '{caller}' lacks write access to '{owner}' workflows"),
            ));
        }
        let exec = self
            .backend
            .describe_workflow(owner, id, pinned_run_id)
            .await
            .map_err(map_backend_err)?;
        if let Some(expected_run) = pinned_run_id
            && exec.run_id != expected_run
        {
            return Err((
                ErrorCode::InvalidArgument,
                "workflow run ID changed before cancellation".into(),
            ));
        }
        let verified = self
            .catalog
            .verify_execution(owner, &exec.task_queue, &exec.workflow_type, &exec.run_id)
            .map_err(|e| (ErrorCode::PermissionDenied, e.detail()))?;
        if !self
            .catalog
            .can_write(caller, owner, verified.worker.name())
        {
            return Err((
                ErrorCode::PermissionDenied,
                format!(
                    "permission denied: '{caller}' lacks write access to '{owner}/{}'",
                    verified.worker.name()
                ),
            ));
        }
        Ok(to_detail(
            owner,
            verified.worker.name(),
            verified.workflow.workflow_type(),
            exec,
        ))
    }

    pub async fn handle_updater_request(&self, req: UpdaterRequest) -> UpdaterResponse {
        match req {
            UpdaterRequest::GateStatus => match self.coordinator.gate_snapshot() {
                Gate::Open => UpdaterResponse::GateStatus {
                    open: true,
                    update_id: None,
                    rev: None,
                },
                Gate::Closed { update_id, rev, .. } => UpdaterResponse::GateStatus {
                    open: false,
                    update_id: Some(update_id),
                    rev: Some(rev),
                },
            },
            UpdaterRequest::CloseGate { update_id, rev } => {
                if let Err(err) = self.coordinator.close_gate(&update_id, &rev).await {
                    return UpdaterResponse::Error(err.detail());
                }
                let sched_outcome = match self.backend.pause_schedules_and_collect().await {
                    Ok(out) => out,
                    Err(err) => {
                        return UpdaterResponse::Error(format!(
                            "failed to coordinate schedules: {}",
                            err.detail()
                        ));
                    }
                };
                if let Err(err) = self
                    .coordinator
                    .add_paused_schedules(&update_id, &sched_outcome.newly_paused)
                {
                    return UpdaterResponse::Error(err.detail());
                }
                if let Err(err) = self.coordinator.track_runs(sched_outcome.discovered_runs) {
                    return UpdaterResponse::Error(err.detail());
                }
                UpdaterResponse::GateClosed
            }
            UpdaterRequest::CheckDrain { update_id } => self.check_drain(&update_id).await,
            UpdaterRequest::ReopenGate { update_id } => {
                let paused = match self.coordinator.require_closed_for(&update_id) {
                    Ok(list) => list,
                    Err(_) if matches!(self.coordinator.gate_snapshot(), Gate::Open) => {
                        return UpdaterResponse::GateReopened;
                    }
                    Err(err) => return UpdaterResponse::Error(err.detail()),
                };
                if let Err(err) = self.backend.unpause_schedules(&paused).await {
                    return UpdaterResponse::Error(format!(
                        "failed to unpause schedules: {}",
                        err.detail()
                    ));
                }
                match self.coordinator.reopen_gate(&update_id) {
                    Ok(()) => UpdaterResponse::GateReopened,
                    Err(err) => UpdaterResponse::Error(err.detail()),
                }
            }
        }
    }

    async fn check_drain(&self, update_id: &str) -> UpdaterResponse {
        if let Err(err) = self.coordinator.require_closed_for(update_id) {
            return UpdaterResponse::Error(err.detail());
        }

        let sched_runs = match self.backend.verify_all_schedules_paused().await {
            Ok(r) => r,
            Err(err) => {
                return UpdaterResponse::Error(format!(
                    "schedule verification failed: {}",
                    err.detail()
                ));
            }
        };
        if let Err(err) = self.coordinator.track_runs(sched_runs) {
            return UpdaterResponse::Error(err.detail());
        }

        let vis_runs = match self.backend.list_running_visibility().await {
            Ok(r) => r,
            Err(err) => {
                return UpdaterResponse::Error(format!(
                    "visibility reconciliation failed: {}",
                    err.detail()
                ));
            }
        };
        if let Err(err) = self.coordinator.track_runs(vis_runs) {
            return UpdaterResponse::Error(err.detail());
        }

        let mut inspected = BTreeSet::new();
        let mut to_retire = Vec::new();
        let mut active_workflows = 0u32;
        let mut active_names = Vec::new();

        for _ in 0..16 {
            let snapshot = self.coordinator.tracked_runs_snapshot();
            let mut discovered_new = false;
            for key in snapshot {
                if !inspected.insert(key.clone()) {
                    continue;
                }
                let desc = match self
                    .backend
                    .describe_workflow(&key.namespace, &key.workflow_id, key.run_id.as_deref())
                    .await
                {
                    Ok(d) => d,
                    Err(BackendError::NotFound(_)) => {
                        to_retire.push(key);
                        continue;
                    }
                    Err(err) => {
                        return UpdaterResponse::Error(format!(
                            "describe {}/{}: {}",
                            key.namespace,
                            key.workflow_id,
                            err.detail()
                        ));
                    }
                };

                let child_keys: Vec<TrackedKey> = desc
                    .pending_children
                    .iter()
                    .filter(|(wf, _)| !wf.is_empty())
                    .map(|(wf, rid)| TrackedKey {
                        namespace: key.namespace.clone(),
                        workflow_id: wf.clone(),
                        run_id: if rid.is_empty() {
                            None
                        } else {
                            Some(rid.clone())
                        },
                    })
                    .collect();
                if !child_keys.is_empty() {
                    if let Err(err) = self.coordinator.track_runs(child_keys) {
                        return UpdaterResponse::Error(err.detail());
                    }
                    discovered_new = true;
                }

                if desc.status.is_open() || desc.has_pending_work {
                    active_workflows += 1;
                    active_names.push(format!("{}/{}", key.namespace, desc.workflow_id));
                    continue;
                }

                let descendants = match self
                    .backend
                    .scan_history_descendants(&key.namespace, &desc.workflow_id, &desc.run_id)
                    .await
                {
                    Ok(list) => list,
                    Err(err) => {
                        return UpdaterResponse::Error(format!(
                            "history scan {}/{}: {}",
                            key.namespace,
                            desc.workflow_id,
                            err.detail()
                        ));
                    }
                };
                let unseen: Vec<TrackedKey> = descendants
                    .into_iter()
                    .filter(|d| !inspected.contains(d))
                    .collect();
                if !unseen.is_empty() {
                    if let Err(err) = self.coordinator.track_runs(unseen) {
                        return UpdaterResponse::Error(err.detail());
                    }
                    discovered_new = true;
                }
                to_retire.push(key);
            }
            if !discovered_new {
                break;
            }
        }

        let db_running = match self.backend.list_running_history_db().await {
            Ok(rows) => rows,
            Err(err) => {
                return UpdaterResponse::Error(format!(
                    "history store check failed: {}",
                    err.detail()
                ));
            }
        };
        if !db_running.is_empty() {
            let db_keys: Vec<TrackedKey> = db_running
                .iter()
                .map(|r| TrackedKey {
                    namespace: r.namespace.clone(),
                    workflow_id: r.workflow_id.clone(),
                    run_id: Some(pg::format_uuid_hex(&r.run_id_hex)),
                })
                .collect();
            let _ = self.coordinator.track_runs(db_keys);
            for row in &db_running {
                let tag = format!("{}/{}", row.namespace, row.workflow_id);
                if !active_names.contains(&tag) {
                    active_workflows += 1;
                    active_names.push(tag);
                }
            }
        }

        if let Err(err) = self.coordinator.retire_runs(&to_retire) {
            return UpdaterResponse::Error(err.detail());
        }

        let active_leases = self.coordinator.active_lease_count() as u32;
        if active_workflows > 0 || active_leases > 0 {
            return UpdaterResponse::DrainBusy {
                active_workflows,
                active_leases,
                detail: format!(
                    "draining: {} active workflow(s) [{}], {} worker lease(s)",
                    active_workflows,
                    active_names.join(", "),
                    active_leases
                ),
            };
        }

        UpdaterResponse::DrainQuiescent
    }
}

fn to_detail(owner: &str, worker: &str, workflow_type: &str, exec: RawExecution) -> WorkflowDetail {
    WorkflowDetail {
        owner: owner.to_owned(),
        worker: worker.to_owned(),
        workflow_type: workflow_type.to_owned(),
        task_queue: exec.task_queue,
        workflow_id: exec.workflow_id,
        run_id: exec.run_id,
        status: exec.status.label().to_owned(),
        start_time: exec.start_time,
        close_time: exec.close_time,
    }
}

fn map_catalog_err(err: catalog::Error) -> (ErrorCode, String) {
    let code = match err {
        catalog::Error::PermissionDenied(_) | catalog::Error::ExecutionMismatch(_) => {
            ErrorCode::PermissionDenied
        }
        catalog::Error::InvalidArgument(_)
        | catalog::Error::UnknownUser(_)
        | catalog::Error::UnknownWorker { .. } => ErrorCode::InvalidArgument,
        catalog::Error::InvalidCatalog(_) => ErrorCode::Internal,
    };
    (code, err.detail())
}

fn map_ledger_err(err: ledger::Error) -> (ErrorCode, String) {
    match err {
        ledger::Error::AdmissionClosed => (ErrorCode::AdmissionClosed, err.detail()),
        ledger::Error::Conflict(_) | ledger::Error::Storage(_) => {
            (ErrorCode::Internal, err.detail())
        }
    }
}

fn map_backend_err(err: BackendError) -> (ErrorCode, String) {
    let code = match err {
        BackendError::NotFound(_) => ErrorCode::NotFound,
        BackendError::AlreadyExists(_) | BackendError::InvalidArgument(_) => {
            ErrorCode::InvalidArgument
        }
        BackendError::Unavailable(_) => ErrorCode::Unavailable,
        BackendError::Internal(_) => ErrorCode::Internal,
    };
    (code, err.detail())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pg::ActiveDbExecution;
    use crate::temporal::{
        BoxFuture, ExecStatus, RawEvent, RawInspection, RawStep, SchedulePauseOutcome,
    };
    use boreal_common::wire::{StepState, WorkflowEvent, WorkflowStep};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockState {
        runs: HashMap<(String, String), RawExecution>,
        inspections: HashMap<(String, String), RawInspection>,
        descendants: HashMap<(String, String), Vec<TrackedKey>>,
        db_running: Vec<ActiveDbExecution>,
        unpaused_schedules: Vec<(String, String)>,
        fail_db: bool,
    }

    struct MockBackend {
        state: Mutex<MockState>,
    }

    impl Backend for MockBackend {
        fn start_workflow<'a>(
            &'a self,
            namespace: &'a str,
            task_queue: &'a str,
            workflow_type: &'a str,
            workflow_id: &'a str,
            _json_payload: &'a str,
        ) -> BoxFuture<'a, String> {
            let mut guard = self.state.lock().unwrap();
            let run_id = format!("run-{workflow_id}");
            guard.runs.insert(
                (namespace.to_owned(), workflow_id.to_owned()),
                RawExecution {
                    workflow_id: workflow_id.to_owned(),
                    run_id: run_id.clone(),
                    workflow_type: workflow_type.to_owned(),
                    task_queue: task_queue.to_owned(),
                    status: ExecStatus::Running,
                    start_time: "1.000s".into(),
                    close_time: "-".into(),
                    has_pending_work: false,
                    pending_children: Vec::new(),
                },
            );
            Box::pin(async move { Ok(run_id) })
        }

        fn describe_workflow<'a>(
            &'a self,
            namespace: &'a str,
            workflow_id: &'a str,
            _run_id: Option<&'a str>,
        ) -> BoxFuture<'a, RawExecution> {
            let res = self
                .state
                .lock()
                .unwrap()
                .runs
                .get(&(namespace.to_owned(), workflow_id.to_owned()))
                .cloned()
                .ok_or_else(|| BackendError::NotFound("not found".into()));
            Box::pin(async move { res })
        }

        fn list_workflows<'a>(&'a self, namespace: &'a str) -> BoxFuture<'a, Vec<RawExecution>> {
            let list: Vec<_> = self
                .state
                .lock()
                .unwrap()
                .runs
                .iter()
                .filter(|((ns, _), _)| ns == namespace)
                .map(|(_, v)| v.clone())
                .collect();
            Box::pin(async move { Ok(list) })
        }

        fn fetch_close_outcome<'a>(
            &'a self,
            _namespace: &'a str,
            _workflow_id: &'a str,
            _run_id: &'a str,
        ) -> BoxFuture<'a, CloseOutcome> {
            Box::pin(async { Ok(CloseOutcome::Completed("Hello, Alice!".into())) })
        }

        fn inspect_workflow<'a>(
            &'a self,
            namespace: &'a str,
            workflow_id: &'a str,
            _run_id: &'a str,
        ) -> BoxFuture<'a, RawInspection> {
            let insp = self
                .state
                .lock()
                .unwrap()
                .inspections
                .get(&(namespace.to_owned(), workflow_id.to_owned()))
                .cloned()
                .unwrap_or_default();
            Box::pin(async move { Ok(insp) })
        }

        fn cancel_workflow<'a>(
            &'a self,
            namespace: &'a str,
            workflow_id: &'a str,
            _run_id: &'a str,
        ) -> BoxFuture<'a, ()> {
            if let Some(r) = self
                .state
                .lock()
                .unwrap()
                .runs
                .get_mut(&(namespace.to_owned(), workflow_id.to_owned()))
            {
                r.status = ExecStatus::Canceled;
            }
            Box::pin(async { Ok(()) })
        }

        fn pause_schedules_and_collect(&self) -> BoxFuture<'_, SchedulePauseOutcome> {
            let mut guard = self.state.lock().unwrap();
            let newly_paused = std::mem::take(&mut guard.unpaused_schedules);
            Box::pin(async move {
                Ok(SchedulePauseOutcome {
                    newly_paused,
                    discovered_runs: Vec::new(),
                })
            })
        }

        fn verify_all_schedules_paused(&self) -> BoxFuture<'_, Vec<TrackedKey>> {
            let unpaused = !self.state.lock().unwrap().unpaused_schedules.is_empty();
            Box::pin(async move {
                if unpaused {
                    Err(BackendError::Internal("unpaused schedule exists".into()))
                } else {
                    Ok(Vec::new())
                }
            })
        }

        fn unpause_schedules<'a>(&'a self, schedules: &'a [(String, String)]) -> BoxFuture<'a, ()> {
            self.state
                .lock()
                .unwrap()
                .unpaused_schedules
                .extend_from_slice(schedules);
            Box::pin(async { Ok(()) })
        }

        fn scan_history_descendants<'a>(
            &'a self,
            namespace: &'a str,
            workflow_id: &'a str,
            _run_id: &'a str,
        ) -> BoxFuture<'a, Vec<TrackedKey>> {
            let list = self
                .state
                .lock()
                .unwrap()
                .descendants
                .get(&(namespace.to_owned(), workflow_id.to_owned()))
                .cloned()
                .unwrap_or_default();
            Box::pin(async move { Ok(list) })
        }

        fn list_running_visibility(&self) -> BoxFuture<'_, Vec<TrackedKey>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn list_running_history_db(&self) -> BoxFuture<'_, Vec<ActiveDbExecution>> {
            let guard = self.state.lock().unwrap();
            if guard.fail_db {
                return Box::pin(async { Err(BackendError::Unavailable("db down".into())) });
            }
            let rows = guard.db_running.clone();
            Box::pin(async move { Ok(rows) })
        }

        fn count_open_workflows<'a>(&'a self, namespace: &'a str) -> BoxFuture<'a, WorkflowCounts> {
            let guard = self.state.lock().unwrap();
            if guard.fail_db {
                return Box::pin(async { Err(BackendError::Unavailable("db down".into())) });
            }
            let mut paused_ids = BTreeSet::new();
            let mut running_ids = BTreeSet::new();
            for ((ns, id), exec) in &guard.runs {
                if ns == namespace {
                    match exec.status {
                        ExecStatus::Paused => {
                            paused_ids.insert(id.clone());
                        }
                        ExecStatus::Running => {
                            running_ids.insert(id.clone());
                        }
                        _ => {}
                    }
                }
            }
            for row in &guard.db_running {
                if row.namespace == namespace && !paused_ids.contains(&row.workflow_id) {
                    running_ids.insert(row.workflow_id.clone());
                }
            }
            let counts = WorkflowCounts {
                paused: paused_ids.len() as u32,
                running: running_ids.len() as u32,
            };
            Box::pin(async move { Ok(counts) })
        }
    }

    fn demo_catalog() -> Catalog {
        let workers = "\
lab-worker:
  workflow: DemoWorkflow
  description: Greet
  args:
  - name: name
    type: string
  - name: wait_seconds
    type: int
    min: 1
    max: 7200
    default: 20
lab-worker-alt:
  workflow: CountWorkflow
  description: Count
  args:
  - name: name
    type: string
  - name: wait_seconds
    type: int
    min: 1
    max: 7200
    default: 20
";
        let mut users = std::collections::BTreeMap::new();
        users.insert(
            "alice".into(),
            "\
user: alice
permissions:
  read: true
  write: false
workers:
  greet:
    package: lab-worker
    permissions:
      read: true
      write: [bob]
  count:
    package: lab-worker-alt
    permissions:
      read: [bob]
      write: false
"
            .into(),
        );
        users.insert(
            "bob".into(),
            "\
user: bob
permissions:
  read: false
  write: false
workers:
  greet:
    package: lab-worker
    permissions:
      read: false
      write: false
"
            .into(),
        );
        Catalog::from_yaml(workers, &users).unwrap()
    }

    #[tokio::test]
    async fn test_enforces_delegated_permissions_and_workflow_type_verification() {
        let dir = std::env::temp_dir().join(format!("boreal-engine-test-1-{}", std::process::id()));
        let coord = Arc::new(Coordinator::open(&dir).unwrap());
        let backend = Arc::new(MockBackend {
            state: Mutex::new(MockState::default()),
        });
        let engine = Engine::new(demo_catalog(), coord, Arc::clone(&backend));

        // Bob is delegated write on alice/greet, but NOT alice/count.
        let started = engine
            .handle_user_request(
                "bob",
                Request::Run {
                    owner: "alice".into(),
                    worker: "greet".into(),
                    workflow: None,
                    id: "g1".into(),
                    input: RunInput::Json(r#"{"name":"Alice","wait_seconds":5}"#.into()),
                },
            )
            .await;
        assert!(matches!(started, Response::Started { .. }));

        let denied = engine
            .handle_user_request(
                "bob",
                Request::Run {
                    owner: "alice".into(),
                    worker: "count".into(),
                    workflow: None,
                    id: "c1".into(),
                    input: RunInput::Positional(vec!["Alice".into()]),
                },
            )
            .await;
        assert!(matches!(
            denied,
            Response::Error {
                code: ErrorCode::PermissionDenied,
                ..
            }
        ));

        // Alice starts alice/count; Bob can read status, but CANNOT cancel it.
        engine
            .handle_user_request(
                "alice",
                Request::Run {
                    owner: "alice".into(),
                    worker: "count".into(),
                    workflow: None,
                    id: "c1".into(),
                    input: RunInput::Positional(vec!["Alice".into()]),
                },
            )
            .await;
        assert!(matches!(
            engine
                .handle_user_request(
                    "bob",
                    Request::Status {
                        owner: "alice".into(),
                        id: "c1".into()
                    }
                )
                .await,
            Response::Status(_)
        ));
        assert!(matches!(
            engine
                .handle_user_request(
                    "bob",
                    Request::PrepareCancel {
                        owner: "alice".into(),
                        id: "c1".into()
                    }
                )
                .await,
            Response::Error {
                code: ErrorCode::PermissionDenied,
                ..
            }
        ));

        // Spoofed/mismatched workflowType in Temporal must fail closed!
        backend
            .state
            .lock()
            .unwrap()
            .runs
            .get_mut(&("alice".into(), "g1".into()))
            .unwrap()
            .workflow_type = "UnregisteredWorkflow".into();
        assert!(matches!(
            engine
                .handle_user_request(
                    "alice",
                    Request::Status {
                        owner: "alice".into(),
                        id: "g1".into()
                    }
                )
                .await,
            Response::Error {
                code: ErrorCode::PermissionDenied,
                ..
            }
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_drain_covers_schedules_abandoned_children_db_and_uncertainty() {
        let dir = std::env::temp_dir().join(format!("boreal-engine-test-2-{}", std::process::id()));
        let coord = Arc::new(Coordinator::open(&dir).unwrap());
        let backend = Arc::new(MockBackend {
            state: Mutex::new(MockState {
                unpaused_schedules: vec![("alice".into(), "hourly".into())],
                ..Default::default()
            }),
        });
        let engine = Engine::new(demo_catalog(), Arc::clone(&coord), Arc::clone(&backend));

        // Start parent workflow p1, which spawned abandoned child c1 (hidden from Visibility!).
        engine
            .handle_user_request(
                "alice",
                Request::Run {
                    owner: "alice".into(),
                    worker: "greet".into(),
                    workflow: None,
                    id: "p1".into(),
                    input: RunInput::Positional(vec!["Parent".into()]),
                },
            )
            .await;
        {
            let mut st = backend.state.lock().unwrap();
            st.runs
                .get_mut(&("alice".into(), "p1".into()))
                .unwrap()
                .status = ExecStatus::Completed;
            st.descendants.insert(
                ("alice".into(), "p1".into()),
                vec![TrackedKey {
                    namespace: "alice".into(),
                    workflow_id: "c1".into(),
                    run_id: Some("run-c1".into()),
                }],
            );
            st.runs.insert(
                ("alice".into(), "c1".into()),
                RawExecution {
                    workflow_id: "c1".into(),
                    run_id: "run-c1".into(),
                    workflow_type: "DemoWorkflow".into(),
                    task_queue: "alice-greet".into(),
                    status: ExecStatus::Running,
                    start_time: "1.0s".into(),
                    close_time: "-".into(),
                    has_pending_work: false,
                    pending_children: Vec::new(),
                },
            );
        }

        assert_eq!(
            engine
                .handle_updater_request(UpdaterRequest::CloseGate {
                    update_id: "u1".into(),
                    rev: "0123".into(),
                })
                .await,
            UpdaterResponse::GateClosed
        );

        // New starts must be rejected while gate is closed.
        assert!(matches!(
            engine
                .handle_user_request(
                    "alice",
                    Request::Run {
                        owner: "alice".into(),
                        worker: "greet".into(),
                        workflow: None,
                        id: "blocked".into(),
                        input: RunInput::Positional(vec!["X".into()]),
                    },
                )
                .await,
            Response::Error {
                code: ErrorCode::AdmissionClosed,
                ..
            }
        ));

        // Even though p1 completed, history scan discovers running child c1!
        assert!(matches!(
            engine
                .handle_updater_request(UpdaterRequest::CheckDrain {
                    update_id: "u1".into()
                })
                .await,
            UpdaterResponse::DrainBusy {
                active_workflows: 1,
                ..
            }
        ));

        // Complete c1, but simulate DB failure -> must fail closed (Error, NOT Quiescent)!
        {
            let mut st = backend.state.lock().unwrap();
            st.runs
                .get_mut(&("alice".into(), "c1".into()))
                .unwrap()
                .status = ExecStatus::Completed;
            st.fail_db = true;
        }
        assert!(matches!(
            engine
                .handle_updater_request(UpdaterRequest::CheckDrain {
                    update_id: "u1".into()
                })
                .await,
            UpdaterResponse::Error(_)
        ));

        // DB recovers -> now DrainQuiescent!
        backend.state.lock().unwrap().fail_db = false;
        assert_eq!(
            engine
                .handle_updater_request(UpdaterRequest::CheckDrain {
                    update_id: "u1".into()
                })
                .await,
            UpdaterResponse::DrainQuiescent
        );

        // Reopening unpauses the schedule that CloseGate paused.
        assert_eq!(
            engine
                .handle_updater_request(UpdaterRequest::ReopenGate {
                    update_id: "u1".into()
                })
                .await,
            UpdaterResponse::GateReopened
        );
        assert_eq!(
            backend.state.lock().unwrap().unpaused_schedules,
            vec![("alice".into(), "hourly".into())]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_summary_isolates_caller_counts_and_degrades_gracefully() {
        let dir = std::env::temp_dir().join(format!("boreal-engine-test-3-{}", std::process::id()));
        let coord = Arc::new(Coordinator::open(&dir).unwrap());
        let backend = Arc::new(MockBackend {
            state: Mutex::new(MockState::default()),
        });
        let engine = Engine::new(demo_catalog(), coord, Arc::clone(&backend));

        assert_eq!(
            engine.handle_user_request("alice", Request::Summary).await,
            Response::Summary(PromptSummary {
                workflows: Some(WorkflowCounts {
                    paused: 0,
                    running: 0,
                }),
                draining: false,
            })
        );

        engine
            .handle_user_request(
                "alice",
                Request::Run {
                    owner: "alice".into(),
                    worker: "greet".into(),
                    workflow: None,
                    id: "a1".into(),
                    input: RunInput::Positional(vec!["Alice".into()]),
                },
            )
            .await;
        engine
            .handle_user_request(
                "alice",
                Request::Run {
                    owner: "alice".into(),
                    worker: "greet".into(),
                    workflow: None,
                    id: "a2".into(),
                    input: RunInput::Positional(vec!["Ada".into()]),
                },
            )
            .await;
        backend
            .state
            .lock()
            .unwrap()
            .runs
            .get_mut(&("alice".into(), "a2".into()))
            .unwrap()
            .status = ExecStatus::Paused;

        // Alice sees 1 paused + 1 running in her namespace; Bob still sees 0/0 in his own.
        assert_eq!(
            engine.handle_user_request("alice", Request::Summary).await,
            Response::Summary(PromptSummary {
                workflows: Some(WorkflowCounts {
                    paused: 1,
                    running: 1,
                }),
                draining: false,
            })
        );
        assert_eq!(
            engine.handle_user_request("bob", Request::Summary).await,
            Response::Summary(PromptSummary {
                workflows: Some(WorkflowCounts {
                    paused: 0,
                    running: 0,
                }),
                draining: false,
            })
        );

        // Gate closure surfaces draining=true, and DB outage degrades workflows to None.
        assert_eq!(
            engine
                .handle_updater_request(UpdaterRequest::CloseGate {
                    update_id: "u-sum".into(),
                    rev: "abcd".into(),
                })
                .await,
            UpdaterResponse::GateClosed
        );
        backend.state.lock().unwrap().fail_db = true;
        assert_eq!(
            engine.handle_user_request("alice", Request::Summary).await,
            Response::Summary(PromptSummary {
                workflows: None,
                draining: true,
            })
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_view_returns_steps_events_and_redacts_unauthorized_child_workers() {
        let mut users = std::collections::BTreeMap::new();
        users.insert(
            "alice".into(),
            "user: alice\npermissions: {read: true, write: false}\nworkers:\n  greet:\n    package: lab-worker\n    permissions: {read: true, write: false}\n  count:\n    package: lab-worker-alt\n    permissions: {read: [bob], write: false}\n".into(),
        );
        users.insert(
            "bob".into(),
            "user: bob\npermissions: {read: false, write: false}\nworkers: {}\n".into(),
        );
        users.insert(
            "carol".into(),
            "user: carol\npermissions: {read: false, write: false}\nworkers: {}\n".into(),
        );
        let workers = "lab-worker:\n  workflow: DemoWorkflow\n  args:\n  - name: name\n    type: string\nlab-worker-alt:\n  workflow: CountWorkflow\n";
        let catalog = Catalog::from_yaml(workers, &users).unwrap();

        let dir = std::env::temp_dir().join(format!("boreal-engine-test-4-{}", std::process::id()));
        let coord = Arc::new(Coordinator::open(&dir).unwrap());
        let backend = Arc::new(MockBackend {
            state: Mutex::new(MockState::default()),
        });
        let engine = Engine::new(catalog, coord, Arc::clone(&backend));

        engine
            .handle_user_request(
                "alice",
                Request::Run {
                    owner: "alice".into(),
                    worker: "greet".into(),
                    workflow: None,
                    id: "v1".into(),
                    input: RunInput::Positional(vec!["Alice".into()]),
                },
            )
            .await;

        backend.state.lock().unwrap().inspections.insert(
            ("alice".into(), "v1".into()),
            RawInspection {
                elapsed: "2.50s".into(),
                input: r#"{"name":"Alice"}"#.into(),
                task_warning: String::new(),
                steps: vec![RawStep {
                    step: WorkflowStep {
                        kind: "child".into(),
                        name: "CountWorkflow 'v1-child'".into(),
                        state: StepState::Failed,
                        duration: "1.10s".into(),
                        detail: "private child failure".into(),
                    },
                    child_queue: Some(("alice".into(), "alice-count".into())),
                }],
                events: vec![RawEvent {
                    event: WorkflowEvent {
                        id: 2,
                        timestamp: "14:02:11.000".into(),
                        kind: "ChildFailed".into(),
                        summary: "private child failure".into(),
                    },
                    child_queue: Some(("alice".into(), "alice-count".into())),
                }],
                outcome: None,
            },
        );

        // Bob can read both alice/greet and alice/count -> sees child detail unredacted.
        let Response::View(bob_view) = engine
            .handle_user_request(
                "bob",
                Request::View {
                    owner: "alice".into(),
                    id: "v1".into(),
                },
            )
            .await
        else {
            panic!("expected Response::View for bob");
        };
        assert_eq!(bob_view.steps[0].detail, "private child failure");
        assert_eq!(bob_view.events[0].summary, "private child failure");

        // Carol can read alice/greet, but NOT alice/count -> child payload is redacted!
        let Response::View(carol_view) = engine
            .handle_user_request(
                "carol",
                Request::View {
                    owner: "alice".into(),
                    id: "v1".into(),
                },
            )
            .await
        else {
            panic!("expected Response::View for carol");
        };
        assert_eq!(carol_view.steps[0].detail, "[redacted]");
        assert_eq!(carol_view.events[0].summary, "[redacted]");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
