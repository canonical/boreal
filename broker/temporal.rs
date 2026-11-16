use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use temporalio_common::protos::temporal::api::{
    common::v1::{Payload, Payloads, WorkflowExecution, WorkflowType},
    enums::v1::{HistoryEventFilterType, TaskQueueKind, WorkflowExecutionStatus},
    failure::v1::Failure,
    history::v1::history_event::Attributes as EventAttrs,
    schedule::v1::SchedulePatch,
    taskqueue::v1::TaskQueue,
    workflow::v1::WorkflowExecutionInfo,
    workflowservice::v1::{
        DescribeScheduleRequest, DescribeWorkflowExecutionRequest,
        GetWorkflowExecutionHistoryRequest, ListNamespacesRequest, ListSchedulesRequest,
        ListWorkflowExecutionsRequest, PatchScheduleRequest, RequestCancelWorkflowExecutionRequest,
        StartWorkflowExecutionRequest, workflow_service_client::WorkflowServiceClient,
    },
};
use tonic::{
    Code,
    transport::{Channel, Endpoint},
};
use uuid::Uuid;

use crate::ledger::TrackedKey;
use crate::pg::{self, ActiveDbExecution};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BackendError>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub enum BackendError {
    NotFound(String),
    AlreadyExists(String),
    InvalidArgument(String),
    Unavailable(String),
    Internal(String),
}

impl BackendError {
    pub fn detail(&self) -> String {
        match self {
            Self::NotFound(m)
            | Self::AlreadyExists(m)
            | Self::InvalidArgument(m)
            | Self::Unavailable(m)
            | Self::Internal(m) => m.clone(),
        }
    }

    fn from_status(status: tonic::Status) -> Self {
        let msg = status.message().to_owned();
        match status.code() {
            Code::NotFound => Self::NotFound(msg),
            Code::AlreadyExists => Self::AlreadyExists(msg),
            Code::InvalidArgument => Self::InvalidArgument(msg),
            Code::Unavailable | Code::DeadlineExceeded => Self::Unavailable(msg),
            _ => Self::Internal(format!("{}: {msg}", status.code())),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Canceled,
    Terminated,
    ContinuedAsNew,
    TimedOut,
    Unspecified,
}

impl ExecStatus {
    pub fn from_proto(code: i32) -> Self {
        match WorkflowExecutionStatus::try_from(code)
            .unwrap_or(WorkflowExecutionStatus::Unspecified)
        {
            WorkflowExecutionStatus::Running => Self::Running,
            WorkflowExecutionStatus::Paused => Self::Paused,
            WorkflowExecutionStatus::Completed => Self::Completed,
            WorkflowExecutionStatus::Failed => Self::Failed,
            WorkflowExecutionStatus::Canceled => Self::Canceled,
            WorkflowExecutionStatus::Terminated => Self::Terminated,
            WorkflowExecutionStatus::ContinuedAsNew => Self::ContinuedAsNew,
            WorkflowExecutionStatus::TimedOut => Self::TimedOut,
            WorkflowExecutionStatus::Unspecified => Self::Unspecified,
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "cancelled",
            Self::Terminated => "terminated",
            Self::ContinuedAsNew => "continued_as_new",
            Self::TimedOut => "timed_out",
            Self::Unspecified => "unspecified",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RawExecution {
    pub workflow_id: String,
    pub run_id: String,
    pub workflow_type: String,
    pub task_queue: String,
    pub status: ExecStatus,
    pub start_time: String,
    pub close_time: String,
    pub has_pending_work: bool,
    pub pending_children: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub enum CloseOutcome {
    Completed(String),
    Running,
    Unsuccessful { status: String, detail: String },
}

#[derive(Clone, Debug, Default)]
pub struct SchedulePauseOutcome {
    pub newly_paused: Vec<(String, String)>,
    pub discovered_runs: Vec<TrackedKey>,
}

pub trait Backend: Send + Sync {
    fn start_workflow<'a>(
        &'a self,
        namespace: &'a str,
        task_queue: &'a str,
        workflow_type: &'a str,
        workflow_id: &'a str,
        json_payload: &'a str,
    ) -> BoxFuture<'a, String>;

    fn describe_workflow<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: Option<&'a str>,
    ) -> BoxFuture<'a, RawExecution>;

    fn list_workflows<'a>(&'a self, namespace: &'a str) -> BoxFuture<'a, Vec<RawExecution>>;

    fn fetch_close_outcome<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: &'a str,
    ) -> BoxFuture<'a, CloseOutcome>;

    fn cancel_workflow<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: &'a str,
    ) -> BoxFuture<'a, ()>;

    fn pause_schedules_and_collect(&self) -> BoxFuture<'_, SchedulePauseOutcome>;

    fn verify_all_schedules_paused(&self) -> BoxFuture<'_, Vec<TrackedKey>>;

    fn unpause_schedules<'a>(&'a self, schedules: &'a [(String, String)]) -> BoxFuture<'a, ()>;

    fn scan_history_descendants<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: &'a str,
    ) -> BoxFuture<'a, Vec<TrackedKey>>;

    fn list_running_visibility(&self) -> BoxFuture<'_, Vec<TrackedKey>>;

    fn list_running_history_db(&self) -> BoxFuture<'_, Vec<ActiveDbExecution>>;

    fn count_running_workflows<'a>(&'a self, namespace: &'a str) -> BoxFuture<'a, u32>;
}

pub struct LiveBackend {
    endpoint_uri: String,
    pg_socket: PathBuf,
    pg_user: String,
    pg_db: String,
}

impl LiveBackend {
    pub fn new(temporal_addr: &str, pg_socket: PathBuf, pg_user: String, pg_db: String) -> Self {
        let endpoint_uri =
            if temporal_addr.starts_with("http://") || temporal_addr.starts_with("https://") {
                temporal_addr.to_owned()
            } else {
                format!("http://{temporal_addr}")
            };
        Self {
            endpoint_uri,
            pg_socket,
            pg_user,
            pg_db,
        }
    }

    async fn client(&self) -> Result<WorkflowServiceClient<Channel>, BackendError> {
        let endpoint = Endpoint::from_shared(self.endpoint_uri.clone())
            .map_err(|e| BackendError::Internal(format!("invalid temporal URI: {e}")))?
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20));
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| BackendError::Unavailable(format!("connect temporal: {e}")))?;
        Ok(WorkflowServiceClient::new(channel))
    }

    async fn user_namespaces(
        &self,
        client: &mut WorkflowServiceClient<Channel>,
    ) -> Result<Vec<String>, BackendError> {
        let mut token = Vec::new();
        let mut names = Vec::new();
        loop {
            let resp = client
                .list_namespaces(ListNamespacesRequest {
                    page_size: 100,
                    next_page_token: token,
                    namespace_filter: None,
                })
                .await
                .map_err(BackendError::from_status)?
                .into_inner();
            for ns in resp.namespaces {
                if let Some(info) = ns.namespace_info
                    && !info.name.is_empty()
                    && info.name != "temporal-system"
                {
                    names.push(info.name);
                }
            }
            if resp.next_page_token.is_empty() {
                break;
            }
            token = resp.next_page_token;
        }
        Ok(names)
    }
}

fn format_ts(ts: Option<(i64, i32)>) -> String {
    match ts {
        Some((seconds, nanos)) if seconds > 0 => {
            format!("{seconds}.{:03}s", (nanos.unsigned_abs()) / 1_000_000)
        }
        _ => "-".into(),
    }
}

fn info_to_raw(
    info: WorkflowExecutionInfo,
    has_pending_work: bool,
    pending_children: Vec<(String, String)>,
) -> RawExecution {
    let (workflow_id, run_id) = info
        .execution
        .map(|e| (e.workflow_id, e.run_id))
        .unwrap_or_default();
    let workflow_type = info.r#type.map(|t| t.name).unwrap_or_default();
    RawExecution {
        workflow_id,
        run_id,
        workflow_type,
        task_queue: info.task_queue,
        status: ExecStatus::from_proto(info.status),
        start_time: format_ts(info.start_time.map(|t| (t.seconds, t.nanos))),
        close_time: format_ts(info.close_time.map(|t| (t.seconds, t.nanos))),
        has_pending_work,
        pending_children,
    }
}

fn decode_payloads(payloads: Option<Payloads>) -> String {
    let Some(ps) = payloads else {
        return String::new();
    };
    let mut parts = Vec::with_capacity(ps.payloads.len());
    for payload in ps.payloads {
        if let Ok(text) = std::str::from_utf8(&payload.data) {
            if let Ok(serde_json::Value::String(s)) =
                serde_json::from_str::<serde_json::Value>(text)
            {
                parts.push(s);
            } else {
                parts.push(text.to_owned());
            }
        } else {
            parts.push(format!("<{} binary bytes>", payload.data.len()));
        }
    }
    parts.join("\n")
}

fn format_failure_chain(failure: Option<Failure>) -> String {
    let mut parts = Vec::new();
    let mut cur = failure.map(Box::new);
    while let Some(f) = cur {
        if !f.message.is_empty() {
            parts.push(f.message);
        }
        cur = f.cause;
    }
    if parts.is_empty() {
        "workflow did not complete successfully".into()
    } else {
        parts.join(": ")
    }
}

impl Backend for LiveBackend {
    fn start_workflow<'a>(
        &'a self,
        namespace: &'a str,
        task_queue: &'a str,
        workflow_type: &'a str,
        workflow_id: &'a str,
        json_payload: &'a str,
    ) -> BoxFuture<'a, String> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let mut metadata = HashMap::new();
            metadata.insert("encoding".to_owned(), b"json/plain".to_vec());
            let payload = Payload {
                metadata,
                data: json_payload.as_bytes().to_vec(),
                external_payloads: Vec::new(),
            };
            let resp = client
                .start_workflow_execution(StartWorkflowExecutionRequest {
                    namespace: namespace.to_owned(),
                    workflow_id: workflow_id.to_owned(),
                    workflow_type: Some(WorkflowType {
                        name: workflow_type.to_owned(),
                    }),
                    task_queue: Some(TaskQueue {
                        name: task_queue.to_owned(),
                        kind: TaskQueueKind::Normal as i32,
                        normal_name: String::new(),
                    }),
                    input: Some(Payloads {
                        payloads: vec![payload],
                    }),
                    identity: "boreal-broker".into(),
                    request_id: Uuid::new_v4().to_string(),
                    ..Default::default()
                })
                .await
                .map_err(BackendError::from_status)?
                .into_inner();
            Ok(resp.run_id)
        })
    }

    fn describe_workflow<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: Option<&'a str>,
    ) -> BoxFuture<'a, RawExecution> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let resp = client
                .describe_workflow_execution(DescribeWorkflowExecutionRequest {
                    namespace: namespace.to_owned(),
                    execution: Some(WorkflowExecution {
                        workflow_id: workflow_id.to_owned(),
                        run_id: run_id.unwrap_or("").to_owned(),
                    }),
                })
                .await
                .map_err(BackendError::from_status)?
                .into_inner();
            let info = resp
                .workflow_execution_info
                .ok_or_else(|| BackendError::Internal("missing workflow_execution_info".into()))?;
            let has_pending_work = !resp.pending_activities.is_empty()
                || !resp.pending_children.is_empty()
                || resp.pending_workflow_task.is_some()
                || !resp.pending_nexus_operations.is_empty();
            let pending_children = resp
                .pending_children
                .into_iter()
                .map(|c| (c.workflow_id, c.run_id))
                .collect();
            Ok(info_to_raw(info, has_pending_work, pending_children))
        })
    }

    fn list_workflows<'a>(&'a self, namespace: &'a str) -> BoxFuture<'a, Vec<RawExecution>> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let mut token = Vec::new();
            let mut out = Vec::new();
            for _ in 0..20 {
                let resp = client
                    .list_workflow_executions(ListWorkflowExecutionsRequest {
                        namespace: namespace.to_owned(),
                        page_size: 200,
                        next_page_token: token,
                        query: String::new(),
                    })
                    .await
                    .map_err(BackendError::from_status)?
                    .into_inner();
                for info in resp.executions {
                    let raw = info_to_raw(info, false, Vec::new());
                    if !raw.workflow_id.starts_with("temporal-sys-") {
                        out.push(raw);
                    }
                }
                if resp.next_page_token.is_empty() {
                    break;
                }
                token = resp.next_page_token;
            }
            Ok(out)
        })
    }

    fn fetch_close_outcome<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: &'a str,
    ) -> BoxFuture<'a, CloseOutcome> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let resp = client
                .get_workflow_execution_history(GetWorkflowExecutionHistoryRequest {
                    namespace: namespace.to_owned(),
                    execution: Some(WorkflowExecution {
                        workflow_id: workflow_id.to_owned(),
                        run_id: run_id.to_owned(),
                    }),
                    maximum_page_size: 10,
                    next_page_token: Vec::new(),
                    wait_new_event: false,
                    history_event_filter_type: HistoryEventFilterType::CloseEvent as i32,
                    skip_archival: true,
                })
                .await
                .map_err(BackendError::from_status)?
                .into_inner();
            let Some(history) = resp.history else {
                return Ok(CloseOutcome::Running);
            };
            let Some(event) = history.events.into_iter().last() else {
                return Ok(CloseOutcome::Running);
            };
            match event.attributes {
                Some(EventAttrs::WorkflowExecutionCompletedEventAttributes(attr)) => {
                    Ok(CloseOutcome::Completed(decode_payloads(attr.result)))
                }
                Some(EventAttrs::WorkflowExecutionFailedEventAttributes(attr)) => {
                    Ok(CloseOutcome::Unsuccessful {
                        status: "failed".into(),
                        detail: format_failure_chain(attr.failure),
                    })
                }
                Some(EventAttrs::WorkflowExecutionCanceledEventAttributes(attr)) => {
                    let detail = decode_payloads(attr.details);
                    Ok(CloseOutcome::Unsuccessful {
                        status: "cancelled".into(),
                        detail: if detail.is_empty() {
                            "workflow was cancelled".into()
                        } else {
                            detail
                        },
                    })
                }
                Some(EventAttrs::WorkflowExecutionTerminatedEventAttributes(attr)) => {
                    Ok(CloseOutcome::Unsuccessful {
                        status: "terminated".into(),
                        detail: if attr.reason.is_empty() {
                            "workflow was terminated".into()
                        } else {
                            attr.reason
                        },
                    })
                }
                Some(EventAttrs::WorkflowExecutionTimedOutEventAttributes(_)) => {
                    Ok(CloseOutcome::Unsuccessful {
                        status: "timed_out".into(),
                        detail: "workflow timed out".into(),
                    })
                }
                Some(EventAttrs::WorkflowExecutionContinuedAsNewEventAttributes(attr)) => {
                    Ok(CloseOutcome::Unsuccessful {
                        status: "continued_as_new".into(),
                        detail: format!("continued as run {}", attr.new_execution_run_id),
                    })
                }
                _ => Ok(CloseOutcome::Running),
            }
        })
    }

    fn cancel_workflow<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut client = self.client().await?;
            client
                .request_cancel_workflow_execution(RequestCancelWorkflowExecutionRequest {
                    namespace: namespace.to_owned(),
                    workflow_execution: Some(WorkflowExecution {
                        workflow_id: workflow_id.to_owned(),
                        run_id: run_id.to_owned(),
                    }),
                    identity: "boreal-broker".into(),
                    request_id: Uuid::new_v4().to_string(),
                    first_execution_run_id: String::new(),
                    reason: "cancelled via boreal-shell".into(),
                    links: Vec::new(),
                })
                .await
                .map_err(BackendError::from_status)?;
            Ok(())
        })
    }

    fn pause_schedules_and_collect(&self) -> BoxFuture<'_, SchedulePauseOutcome> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let namespaces = self.user_namespaces(&mut client).await?;
            let mut outcome = SchedulePauseOutcome::default();

            for ns in namespaces {
                let mut token = Vec::new();
                let mut ids = Vec::new();
                loop {
                    let resp = client
                        .list_schedules(ListSchedulesRequest {
                            namespace: ns.clone(),
                            maximum_page_size: 100,
                            next_page_token: token,
                            query: String::new(),
                        })
                        .await
                        .map_err(BackendError::from_status)?
                        .into_inner();
                    for entry in resp.schedules {
                        ids.push(entry.schedule_id);
                    }
                    if resp.next_page_token.is_empty() {
                        break;
                    }
                    token = resp.next_page_token;
                }

                for sched_id in ids {
                    let desc = client
                        .describe_schedule(DescribeScheduleRequest {
                            namespace: ns.clone(),
                            schedule_id: sched_id.clone(),
                        })
                        .await
                        .map_err(BackendError::from_status)?
                        .into_inner();
                    let was_paused = desc
                        .schedule
                        .as_ref()
                        .and_then(|s| s.state.as_ref())
                        .is_some_and(|st| st.paused);
                    if !was_paused {
                        client
                            .patch_schedule(PatchScheduleRequest {
                                namespace: ns.clone(),
                                schedule_id: sched_id.clone(),
                                patch: Some(SchedulePatch {
                                    pause: "boreal-updater admission barrier".into(),
                                    ..Default::default()
                                }),
                                identity: "boreal-broker".into(),
                                request_id: Uuid::new_v4().to_string(),
                            })
                            .await
                            .map_err(BackendError::from_status)?;
                        outcome.newly_paused.push((ns.clone(), sched_id.clone()));
                    }
                    let verify = client
                        .describe_schedule(DescribeScheduleRequest {
                            namespace: ns.clone(),
                            schedule_id: sched_id.clone(),
                        })
                        .await
                        .map_err(BackendError::from_status)?
                        .into_inner();
                    let now_paused = verify
                        .schedule
                        .as_ref()
                        .and_then(|s| s.state.as_ref())
                        .is_some_and(|st| st.paused);
                    if !now_paused {
                        return Err(BackendError::Internal(format!(
                            "schedule '{ns}/{sched_id}' did not enter paused state"
                        )));
                    }
                    collect_schedule_runs(&ns, verify.info, &mut outcome.discovered_runs);
                }
            }
            Ok(outcome)
        })
    }

    fn verify_all_schedules_paused(&self) -> BoxFuture<'_, Vec<TrackedKey>> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let namespaces = self.user_namespaces(&mut client).await?;
            let mut runs = Vec::new();
            for ns in namespaces {
                let mut token = Vec::new();
                loop {
                    let resp = client
                        .list_schedules(ListSchedulesRequest {
                            namespace: ns.clone(),
                            maximum_page_size: 100,
                            next_page_token: token,
                            query: String::new(),
                        })
                        .await
                        .map_err(BackendError::from_status)?
                        .into_inner();
                    for entry in resp.schedules {
                        let desc = client
                            .describe_schedule(DescribeScheduleRequest {
                                namespace: ns.clone(),
                                schedule_id: entry.schedule_id.clone(),
                            })
                            .await
                            .map_err(BackendError::from_status)?
                            .into_inner();
                        let paused = desc
                            .schedule
                            .as_ref()
                            .and_then(|s| s.state.as_ref())
                            .is_some_and(|st| st.paused);
                        if !paused {
                            return Err(BackendError::Internal(format!(
                                "schedule '{}/{}' is unpaused during drain",
                                ns, entry.schedule_id
                            )));
                        }
                        collect_schedule_runs(&ns, desc.info, &mut runs);
                    }
                    if resp.next_page_token.is_empty() {
                        break;
                    }
                    token = resp.next_page_token;
                }
            }
            Ok(runs)
        })
    }

    fn unpause_schedules<'a>(&'a self, schedules: &'a [(String, String)]) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if schedules.is_empty() {
                return Ok(());
            }
            let mut client = self.client().await?;
            for (ns, sched_id) in schedules {
                match client
                    .patch_schedule(PatchScheduleRequest {
                        namespace: ns.clone(),
                        schedule_id: sched_id.clone(),
                        patch: Some(SchedulePatch {
                            unpause: "boreal-updater completed".into(),
                            ..Default::default()
                        }),
                        identity: "boreal-broker".into(),
                        request_id: Uuid::new_v4().to_string(),
                    })
                    .await
                {
                    Ok(_) => {}
                    Err(st) if st.code() == Code::NotFound => {}
                    Err(st) => return Err(BackendError::from_status(st)),
                }
            }
            Ok(())
        })
    }

    fn scan_history_descendants<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: &'a str,
    ) -> BoxFuture<'a, Vec<TrackedKey>> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let mut token = Vec::new();
            let mut out = Vec::new();
            loop {
                let resp = client
                    .get_workflow_execution_history(GetWorkflowExecutionHistoryRequest {
                        namespace: namespace.to_owned(),
                        execution: Some(WorkflowExecution {
                            workflow_id: workflow_id.to_owned(),
                            run_id: run_id.to_owned(),
                        }),
                        maximum_page_size: 200,
                        next_page_token: token,
                        wait_new_event: false,
                        history_event_filter_type: HistoryEventFilterType::AllEvent as i32,
                        skip_archival: true,
                    })
                    .await
                    .map_err(BackendError::from_status)?
                    .into_inner();
                if let Some(history) = resp.history {
                    for event in history.events {
                        match event.attributes {
                            Some(
                                EventAttrs::StartChildWorkflowExecutionInitiatedEventAttributes(a),
                            ) => {
                                let child_ns = if a.namespace.is_empty() {
                                    namespace.to_owned()
                                } else {
                                    a.namespace
                                };
                                if !a.workflow_id.is_empty() {
                                    out.push(TrackedKey {
                                        namespace: child_ns,
                                        workflow_id: a.workflow_id,
                                        run_id: None,
                                    });
                                }
                            }
                            Some(EventAttrs::ChildWorkflowExecutionStartedEventAttributes(a)) => {
                                let child_ns = if a.namespace.is_empty() {
                                    namespace.to_owned()
                                } else {
                                    a.namespace
                                };
                                if let Some(exec) = a.workflow_execution
                                    && !exec.workflow_id.is_empty()
                                {
                                    out.push(TrackedKey {
                                        namespace: child_ns,
                                        workflow_id: exec.workflow_id,
                                        run_id: if exec.run_id.is_empty() {
                                            None
                                        } else {
                                            Some(exec.run_id)
                                        },
                                    });
                                }
                            }
                            Some(EventAttrs::WorkflowExecutionContinuedAsNewEventAttributes(a))
                                if !a.new_execution_run_id.is_empty() =>
                            {
                                out.push(TrackedKey {
                                    namespace: namespace.to_owned(),
                                    workflow_id: workflow_id.to_owned(),
                                    run_id: Some(a.new_execution_run_id),
                                });
                            }
                            Some(EventAttrs::WorkflowExecutionCompletedEventAttributes(a))
                                if !a.new_execution_run_id.is_empty() =>
                            {
                                out.push(TrackedKey {
                                    namespace: namespace.to_owned(),
                                    workflow_id: workflow_id.to_owned(),
                                    run_id: Some(a.new_execution_run_id),
                                });
                            }
                            Some(EventAttrs::WorkflowExecutionFailedEventAttributes(a))
                                if !a.new_execution_run_id.is_empty() =>
                            {
                                out.push(TrackedKey {
                                    namespace: namespace.to_owned(),
                                    workflow_id: workflow_id.to_owned(),
                                    run_id: Some(a.new_execution_run_id),
                                });
                            }
                            Some(EventAttrs::WorkflowExecutionTimedOutEventAttributes(a))
                                if !a.new_execution_run_id.is_empty() =>
                            {
                                out.push(TrackedKey {
                                    namespace: namespace.to_owned(),
                                    workflow_id: workflow_id.to_owned(),
                                    run_id: Some(a.new_execution_run_id),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                if resp.next_page_token.is_empty() {
                    break;
                }
                token = resp.next_page_token;
            }
            Ok(out)
        })
    }

    fn list_running_visibility(&self) -> BoxFuture<'_, Vec<TrackedKey>> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let namespaces = self.user_namespaces(&mut client).await?;
            let mut out = Vec::new();
            for ns in namespaces {
                let mut token = Vec::new();
                loop {
                    let resp = client
                        .list_workflow_executions(ListWorkflowExecutionsRequest {
                            namespace: ns.clone(),
                            page_size: 200,
                            next_page_token: token,
                            query: "ExecutionStatus = 'Running'".into(),
                        })
                        .await
                        .map_err(BackendError::from_status)?
                        .into_inner();
                    for info in resp.executions {
                        if let Some(exec) = info.execution
                            && !exec.workflow_id.is_empty()
                            && !exec.workflow_id.starts_with("temporal-sys-")
                        {
                            out.push(TrackedKey {
                                namespace: ns.clone(),
                                workflow_id: exec.workflow_id,
                                run_id: if exec.run_id.is_empty() {
                                    None
                                } else {
                                    Some(exec.run_id)
                                },
                            });
                        }
                    }
                    if resp.next_page_token.is_empty() {
                        break;
                    }
                    token = resp.next_page_token;
                }
            }
            Ok(out)
        })
    }

    fn list_running_history_db(&self) -> BoxFuture<'_, Vec<ActiveDbExecution>> {
        let sock = self.pg_socket.clone();
        let user = self.pg_user.clone();
        let db = self.pg_db.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || pg::query_running_executions(&sock, &user, &db))
                .await
                .map_err(|e| BackendError::Internal(format!("db task join error: {e}")))?
                .map_err(BackendError::Unavailable)
        })
    }

    fn count_running_workflows<'a>(&'a self, namespace: &'a str) -> BoxFuture<'a, u32> {
        let sock = self.pg_socket.clone();
        let user = self.pg_user.clone();
        let db = self.pg_db.clone();
        let ns = namespace.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                pg::count_running_in_namespace(&sock, &user, &db, &ns)
            })
            .await
            .map_err(|e| BackendError::Internal(format!("db task join error: {e}")))?
            .map_err(BackendError::Unavailable)
        })
    }
}

fn collect_schedule_runs(
    ns: &str,
    info: Option<temporalio_common::protos::temporal::api::schedule::v1::ScheduleInfo>,
    out: &mut Vec<TrackedKey>,
) {
    let Some(info) = info else {
        return;
    };
    for exec in info.running_workflows {
        if !exec.workflow_id.is_empty() {
            out.push(TrackedKey {
                namespace: ns.to_owned(),
                workflow_id: exec.workflow_id,
                run_id: if exec.run_id.is_empty() {
                    None
                } else {
                    Some(exec.run_id)
                },
            });
        }
    }
    for action in info.recent_actions {
        if let Some(exec) = action.start_workflow_result
            && !exec.workflow_id.is_empty()
        {
            out.push(TrackedKey {
                namespace: ns.to_owned(),
                workflow_id: exec.workflow_id,
                run_id: if exec.run_id.is_empty() {
                    None
                } else {
                    Some(exec.run_id)
                },
            });
        }
    }
}
