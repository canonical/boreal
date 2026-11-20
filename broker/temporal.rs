use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use temporalio_common::protos::temporal::api::{
    common::v1::{Payload, Payloads, WorkflowExecution, WorkflowType},
    enums::v1::{HistoryEventFilterType, TaskQueueKind, WorkflowExecutionStatus},
    failure::v1::Failure,
    history::v1::{HistoryEvent, history_event::Attributes as EventAttrs},
    schedule::v1::SchedulePatch,
    taskqueue::v1::TaskQueue,
    workflow::v1::{PendingActivityInfo, WorkflowExecutionInfo},
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

use boreal_common::wire::{self, StepState, WorkflowCounts, WorkflowEvent, WorkflowStep};

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

#[derive(Clone, Debug)]
pub struct RawStep {
    pub step: WorkflowStep,
    pub child_queue: Option<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct RawEvent {
    pub event: WorkflowEvent,
    pub child_queue: Option<(String, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct RawInspection {
    pub elapsed: String,
    pub input: String,
    pub task_warning: String,
    pub steps: Vec<RawStep>,
    pub events: Vec<RawEvent>,
    pub outcome: Option<CloseOutcome>,
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

    fn inspect_workflow<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: &'a str,
    ) -> BoxFuture<'a, RawInspection>;

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

    fn count_open_workflows<'a>(&'a self, namespace: &'a str) -> BoxFuture<'a, WorkflowCounts>;
}

pub struct LiveBackend {
    endpoint_uri: String,
    postgres: Option<PostgresConfig>,
}

pub struct PostgresConfig {
    pub socket: PathBuf,
    pub user: String,
    pub database: String,
}

impl LiveBackend {
    pub fn new(temporal_addr: &str, postgres: Option<PostgresConfig>) -> Self {
        let endpoint_uri =
            if temporal_addr.starts_with("http://") || temporal_addr.starts_with("https://") {
                temporal_addr.to_owned()
            } else {
                format!("http://{temporal_addr}")
            };
        Self {
            endpoint_uri,
            postgres,
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
        Some((seconds, nanos)) if seconds > 0 => wire::format_utc_timestamp(seconds, nanos),
        _ => "-".into(),
    }
}

fn ts_to_ms(ts: Option<(i64, i32)>) -> Option<u64> {
    ts.filter(|(s, _)| *s > 0)
        .map(|(s, n)| (s as u64) * 1_000 + u64::from(n.unsigned_abs() / 1_000_000))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn span_label(start_ms: Option<u64>, end_ms: Option<u64>) -> String {
    match (start_ms, end_ms) {
        (Some(s), Some(e)) => wire::format_duration_ms(e.saturating_sub(s)),
        _ => String::new(),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn one_line_preview(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    truncate_chars(flat.trim(), max)
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

    fn inspect_workflow<'a>(
        &'a self,
        namespace: &'a str,
        workflow_id: &'a str,
        run_id: &'a str,
    ) -> BoxFuture<'a, RawInspection> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let desc = client
                .describe_workflow_execution(DescribeWorkflowExecutionRequest {
                    namespace: namespace.to_owned(),
                    execution: Some(WorkflowExecution {
                        workflow_id: workflow_id.to_owned(),
                        run_id: run_id.to_owned(),
                    }),
                })
                .await
                .map_err(BackendError::from_status)?
                .into_inner();
            let info = desc
                .workflow_execution_info
                .ok_or_else(|| BackendError::Internal("missing workflow_execution_info".into()))?;

            let mut history_events = Vec::new();
            let mut token = Vec::new();
            for _ in 0..10 {
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
                if let Some(h) = resp.history {
                    history_events.extend(h.events);
                }
                if resp.next_page_token.is_empty() {
                    break;
                }
                token = resp.next_page_token;
            }

            let task_attempt = desc.pending_workflow_task.map_or(0, |pwt| pwt.attempt);
            Ok(build_inspection(
                namespace,
                &info,
                task_attempt,
                desc.pending_activities,
                history_events,
                now_unix_ms(),
            ))
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
        Box::pin(async move {
            let pg = self.postgres.as_ref().ok_or_else(|| {
                BackendError::Unavailable("PostgreSQL is not configured for this broker".into())
            })?;
            let (sock, user, db) = (pg.socket.clone(), pg.user.clone(), pg.database.clone());
            tokio::task::spawn_blocking(move || pg::query_running_executions(&sock, &user, &db))
                .await
                .map_err(|e| BackendError::Internal(format!("db task join error: {e}")))?
                .map_err(BackendError::Unavailable)
        })
    }

    fn count_open_workflows<'a>(&'a self, namespace: &'a str) -> BoxFuture<'a, WorkflowCounts> {
        Box::pin(async move {
            let Some(pg) = &self.postgres else {
                return self.count_open_visibility(namespace).await;
            };
            let (sock, user, db) = (pg.socket.clone(), pg.user.clone(), pg.database.clone());
            let ns = namespace.to_owned();
            tokio::task::spawn_blocking(move || pg::count_open_in_namespace(&sock, &user, &db, &ns))
                .await
                .map_err(|e| BackendError::Internal(format!("db task join error: {e}")))?
                .map_err(BackendError::Unavailable)
        })
    }
}

impl LiveBackend {
    // The local dev server has no PostgreSQL history database. This count is
    // for the prompt only; it is not a substitute for the updater's DB check.
    async fn count_open_visibility(&self, namespace: &str) -> Result<WorkflowCounts, BackendError> {
        let mut client = self.client().await?;
        let mut counts = WorkflowCounts::default();
        let mut token = Vec::new();
        loop {
            let resp = client
                .list_workflow_executions(ListWorkflowExecutionsRequest {
                    namespace: namespace.to_owned(),
                    page_size: 200,
                    next_page_token: token,
                    query: "ExecutionStatus IN ('Running', 'Paused')".into(),
                })
                .await
                .map_err(BackendError::from_status)?
                .into_inner();
            for info in resp.executions {
                if info.execution.is_none_or(|exec| {
                    exec.workflow_id.is_empty() || exec.workflow_id.starts_with("temporal-sys-")
                }) {
                    continue;
                }
                match ExecStatus::from_proto(info.status) {
                    ExecStatus::Running => counts.running += 1,
                    ExecStatus::Paused => counts.paused += 1,
                    _ => {}
                }
            }
            token = resp.next_page_token;
            if token.is_empty() {
                return Ok(counts);
            }
        }
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

struct StepSlot {
    kind: &'static str,
    name: String,
    activity_id: String,
    state: StepState,
    start_ms: Option<u64>,
    duration: String,
    detail: String,
    child_queue: Option<(String, String)>,
}

fn build_inspection(
    namespace: &str,
    info: &WorkflowExecutionInfo,
    task_attempt: i32,
    pending_activities: Vec<PendingActivityInfo>,
    history_events: Vec<HistoryEvent>,
    now_ms: u64,
) -> RawInspection {
    let wf_start_ms = ts_to_ms(info.start_time.as_ref().map(|t| (t.seconds, t.nanos)));
    let wf_close_ms = ts_to_ms(info.close_time.as_ref().map(|t| (t.seconds, t.nanos)));
    let elapsed = match (wf_start_ms, wf_close_ms) {
        (Some(s), Some(e)) => wire::format_duration_ms(e.saturating_sub(s)),
        (Some(s), None) => wire::format_duration_ms(now_ms.saturating_sub(s)),
        _ => "-".into(),
    };
    let task_warning = if task_attempt > 1 {
        format!("workflow task retrying (attempt {task_attempt})")
    } else {
        String::new()
    };

    let mut input = String::new();
    let mut outcome = None;
    let mut slots: Vec<StepSlot> = Vec::new();
    let mut by_event: HashMap<i64, usize> = HashMap::new();
    let mut by_timer_id: HashMap<String, usize> = HashMap::new();
    let mut events: Vec<RawEvent> = Vec::new();

    for ev in history_events {
        let ev_id = ev.event_id.max(0) as u64;
        let ev_ts = ev.event_time.as_ref().map(|t| (t.seconds, t.nanos));
        let ev_ms = ts_to_ms(ev_ts);
        let ts_str = ev_ts.map_or_else(
            || "--:--:--.---".into(),
            |(s, n)| wire::format_utc_hms(s, n),
        );
        let Some(attrs) = ev.attributes else {
            continue;
        };

        let mut push_ev = |kind: &str, summary: String, child_queue: Option<(String, String)>| {
            events.push(RawEvent {
                event: WorkflowEvent {
                    id: ev_id,
                    timestamp: ts_str.clone(),
                    kind: kind.to_owned(),
                    summary,
                },
                child_queue,
            });
        };

        match attrs {
            EventAttrs::WorkflowExecutionStartedEventAttributes(a) => {
                input = truncate_chars(&decode_payloads(a.input), 2048);
                let wf_name = a.workflow_type.map(|w| w.name).unwrap_or_default();
                let tq_name = a.task_queue.map(|q| q.name).unwrap_or_default();
                push_ev(
                    "WorkflowStarted",
                    one_line_preview(&format!("{wf_name} on {tq_name}"), 160),
                    None,
                );
            }
            EventAttrs::TimerStartedEventAttributes(a) => {
                let target_ms = a.start_to_fire_timeout.map_or(0, |d| {
                    (d.seconds.max(0) as u64) * 1_000
                        + u64::from(d.nanos.unsigned_abs() / 1_000_000)
                });
                let target_label = wire::format_duration_ms(target_ms);
                let idx = slots.len();
                slots.push(StepSlot {
                    kind: "timer",
                    name: format!("#{} ({target_label})", a.timer_id),
                    activity_id: String::new(),
                    state: StepState::Running,
                    start_ms: ev_ms,
                    duration: String::new(),
                    detail: String::new(),
                    child_queue: None,
                });
                by_event.insert(ev.event_id, idx);
                if !a.timer_id.is_empty() {
                    by_timer_id.insert(a.timer_id.clone(), idx);
                }
                push_ev(
                    "TimerStarted",
                    format!("timer #{} ({target_label})", a.timer_id),
                    None,
                );
            }
            EventAttrs::TimerFiredEventAttributes(a) => {
                let slot_idx = by_event
                    .get(&a.started_event_id)
                    .or_else(|| by_timer_id.get(&a.timer_id))
                    .copied();
                if let Some(idx) = slot_idx {
                    slots[idx].state = StepState::Completed;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                }
                push_ev("TimerFired", format!("timer #{}", a.timer_id), None);
            }
            EventAttrs::TimerCanceledEventAttributes(a) => {
                let slot_idx = by_event
                    .get(&a.started_event_id)
                    .or_else(|| by_timer_id.get(&a.timer_id))
                    .copied();
                if let Some(idx) = slot_idx {
                    slots[idx].state = StepState::Cancelled;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                }
                push_ev("TimerCancelled", format!("timer #{}", a.timer_id), None);
            }
            EventAttrs::ActivityTaskScheduledEventAttributes(a) => {
                let act_name = a
                    .activity_type
                    .map(|t| t.name)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| a.activity_id.clone());
                let idx = slots.len();
                slots.push(StepSlot {
                    kind: "activity",
                    name: act_name.clone(),
                    activity_id: a.activity_id,
                    state: StepState::Pending,
                    start_ms: ev_ms,
                    duration: String::new(),
                    detail: String::new(),
                    child_queue: None,
                });
                by_event.insert(ev.event_id, idx);
                push_ev("ActivityScheduled", act_name, None);
            }
            EventAttrs::ActivityTaskStartedEventAttributes(a) => {
                let mut label = format!("attempt {}", a.attempt);
                if let Some(&idx) = by_event.get(&a.scheduled_event_id) {
                    slots[idx].state = StepState::Running;
                    slots[idx].start_ms = ev_ms.or(slots[idx].start_ms);
                    if a.attempt > 1 {
                        slots[idx].detail = format!("attempt {}", a.attempt);
                    }
                    label = format!("{} (attempt {})", slots[idx].name, a.attempt);
                }
                push_ev("ActivityStarted", label, None);
            }
            EventAttrs::ActivityTaskCompletedEventAttributes(a) => {
                let res_preview = one_line_preview(&decode_payloads(a.result), 120);
                let mut label = res_preview;
                if let Some(&idx) = by_event.get(&a.scheduled_event_id) {
                    slots[idx].state = StepState::Completed;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    slots[idx].detail.clear();
                    label = if label.is_empty() {
                        slots[idx].name.clone()
                    } else {
                        format!("{} -> {label}", slots[idx].name)
                    };
                }
                push_ev("ActivityCompleted", label, None);
            }
            EventAttrs::ActivityTaskFailedEventAttributes(a) => {
                let err = one_line_preview(&format_failure_chain(a.failure), 160);
                let mut label = err.clone();
                if let Some(&idx) = by_event.get(&a.scheduled_event_id) {
                    slots[idx].state = StepState::Failed;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    slots[idx].detail = err;
                    label = format!("{}: {label}", slots[idx].name);
                }
                push_ev("ActivityFailed", label, None);
            }
            EventAttrs::ActivityTaskTimedOutEventAttributes(a) => {
                let err = one_line_preview(&format_failure_chain(a.failure), 160);
                let mut label = err.clone();
                if let Some(&idx) = by_event.get(&a.scheduled_event_id) {
                    slots[idx].state = StepState::TimedOut;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    slots[idx].detail = err;
                    label = format!("{}: {label}", slots[idx].name);
                }
                push_ev("ActivityTimedOut", label, None);
            }
            EventAttrs::ActivityTaskCancelRequestedEventAttributes(a) => {
                let name = by_event
                    .get(&a.scheduled_event_id)
                    .map_or_else(String::new, |&idx| slots[idx].name.clone());
                push_ev("ActivityCancelRequested", name, None);
            }
            EventAttrs::ActivityTaskCanceledEventAttributes(a) => {
                let mut name = String::new();
                if let Some(&idx) = by_event.get(&a.scheduled_event_id) {
                    slots[idx].state = StepState::Cancelled;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    name.clone_from(&slots[idx].name);
                }
                push_ev("ActivityCancelled", name, None);
            }
            EventAttrs::StartChildWorkflowExecutionInitiatedEventAttributes(a) => {
                let wf_name = a.workflow_type.map(|w| w.name).unwrap_or_default();
                let child_ns = if a.namespace.is_empty() {
                    namespace.to_owned()
                } else {
                    a.namespace
                };
                let child_tq = a.task_queue.map(|q| q.name).unwrap_or_default();
                let queue_tag = Some((child_ns, child_tq));
                let step_name = format!("{wf_name} '{}'", a.workflow_id);
                let idx = slots.len();
                slots.push(StepSlot {
                    kind: "child",
                    name: step_name.clone(),
                    activity_id: String::new(),
                    state: StepState::Pending,
                    start_ms: ev_ms,
                    duration: String::new(),
                    detail: String::new(),
                    child_queue: queue_tag.clone(),
                });
                by_event.insert(ev.event_id, idx);
                push_ev("ChildInitiated", step_name, queue_tag);
            }
            EventAttrs::ChildWorkflowExecutionStartedEventAttributes(a) => {
                let mut label = a
                    .workflow_execution
                    .map(|e| e.workflow_id)
                    .unwrap_or_default();
                let mut cq = None;
                if let Some(&idx) = by_event.get(&a.initiated_event_id) {
                    slots[idx].state = StepState::Running;
                    slots[idx].start_ms = ev_ms.or(slots[idx].start_ms);
                    label.clone_from(&slots[idx].name);
                    cq.clone_from(&slots[idx].child_queue);
                }
                push_ev("ChildStarted", label, cq);
            }
            EventAttrs::ChildWorkflowExecutionCompletedEventAttributes(a) => {
                let res_preview = one_line_preview(&decode_payloads(a.result), 120);
                let mut label = res_preview;
                let mut cq = None;
                if let Some(&idx) = by_event.get(&a.initiated_event_id) {
                    slots[idx].state = StepState::Completed;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    cq.clone_from(&slots[idx].child_queue);
                    label = if label.is_empty() {
                        slots[idx].name.clone()
                    } else {
                        format!("{} -> {label}", slots[idx].name)
                    };
                }
                push_ev("ChildCompleted", label, cq);
            }
            EventAttrs::ChildWorkflowExecutionFailedEventAttributes(a) => {
                let err = one_line_preview(&format_failure_chain(a.failure), 160);
                let mut label = err.clone();
                let mut cq = None;
                if let Some(&idx) = by_event.get(&a.initiated_event_id) {
                    slots[idx].state = StepState::Failed;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    slots[idx].detail = err;
                    cq.clone_from(&slots[idx].child_queue);
                    label = format!("{}: {label}", slots[idx].name);
                }
                push_ev("ChildFailed", label, cq);
            }
            EventAttrs::ChildWorkflowExecutionCanceledEventAttributes(a) => {
                let mut label = String::new();
                let mut cq = None;
                if let Some(&idx) = by_event.get(&a.initiated_event_id) {
                    slots[idx].state = StepState::Cancelled;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    label.clone_from(&slots[idx].name);
                    cq.clone_from(&slots[idx].child_queue);
                }
                push_ev("ChildCancelled", label, cq);
            }
            EventAttrs::ChildWorkflowExecutionTimedOutEventAttributes(a) => {
                let mut label = String::new();
                let mut cq = None;
                if let Some(&idx) = by_event.get(&a.initiated_event_id) {
                    slots[idx].state = StepState::TimedOut;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    label.clone_from(&slots[idx].name);
                    cq.clone_from(&slots[idx].child_queue);
                }
                push_ev("ChildTimedOut", label, cq);
            }
            EventAttrs::ChildWorkflowExecutionTerminatedEventAttributes(a) => {
                let mut label = String::new();
                let mut cq = None;
                if let Some(&idx) = by_event.get(&a.initiated_event_id) {
                    slots[idx].state = StepState::Failed;
                    slots[idx].duration = span_label(slots[idx].start_ms, ev_ms);
                    label.clone_from(&slots[idx].name);
                    cq.clone_from(&slots[idx].child_queue);
                }
                push_ev("ChildTerminated", label, cq);
            }
            EventAttrs::WorkflowExecutionSignaledEventAttributes(a) => {
                push_ev(
                    "WorkflowSignaled",
                    one_line_preview(&a.signal_name, 120),
                    None,
                );
            }
            EventAttrs::WorkflowExecutionCancelRequestedEventAttributes(a) => {
                push_ev("CancelRequested", one_line_preview(&a.cause, 160), None);
            }
            EventAttrs::WorkflowExecutionCompletedEventAttributes(a) => {
                let output = decode_payloads(a.result);
                push_ev("WorkflowCompleted", one_line_preview(&output, 160), None);
                outcome = Some(CloseOutcome::Completed(output));
            }
            EventAttrs::WorkflowExecutionFailedEventAttributes(a) => {
                let detail = format_failure_chain(a.failure);
                push_ev("WorkflowFailed", one_line_preview(&detail, 160), None);
                outcome = Some(CloseOutcome::Unsuccessful {
                    status: "failed".into(),
                    detail,
                });
            }
            EventAttrs::WorkflowExecutionCanceledEventAttributes(a) => {
                let raw = decode_payloads(a.details);
                let detail = if raw.is_empty() {
                    "workflow was cancelled".into()
                } else {
                    raw
                };
                push_ev("WorkflowCancelled", one_line_preview(&detail, 160), None);
                outcome = Some(CloseOutcome::Unsuccessful {
                    status: "cancelled".into(),
                    detail,
                });
            }
            EventAttrs::WorkflowExecutionTerminatedEventAttributes(a) => {
                let detail = if a.reason.is_empty() {
                    "workflow was terminated".into()
                } else {
                    a.reason
                };
                push_ev("WorkflowTerminated", one_line_preview(&detail, 160), None);
                outcome = Some(CloseOutcome::Unsuccessful {
                    status: "terminated".into(),
                    detail,
                });
            }
            EventAttrs::WorkflowExecutionTimedOutEventAttributes(_) => {
                push_ev("WorkflowTimedOut", "workflow timed out".into(), None);
                outcome = Some(CloseOutcome::Unsuccessful {
                    status: "timed_out".into(),
                    detail: "workflow timed out".into(),
                });
            }
            EventAttrs::WorkflowExecutionContinuedAsNewEventAttributes(a) => {
                let detail = format!("continued as run {}", a.new_execution_run_id);
                push_ev("ContinuedAsNew", detail.clone(), None);
                outcome = Some(CloseOutcome::Unsuccessful {
                    status: "continued_as_new".into(),
                    detail,
                });
            }
            _ => {}
        }
    }

    for pa in pending_activities {
        let Some(slot) = slots.iter_mut().rev().find(|s| {
            s.kind == "activity"
                && s.activity_id == pa.activity_id
                && matches!(s.state, StepState::Pending | StepState::Running)
        }) else {
            continue;
        };
        if let Some(started) = pa.last_started_time.as_ref()
            && let Some(ms) = ts_to_ms(Some((started.seconds, started.nanos)))
        {
            slot.start_ms = Some(ms);
        }
        if pa.last_failure.is_some() {
            slot.state = StepState::Retrying;
        } else if pa.attempt >= 1
            && slot.state == StepState::Pending
            && pa.last_started_time.is_some()
        {
            slot.state = StepState::Running;
        }
        let mut details = Vec::new();
        if pa.attempt > 1 || pa.maximum_attempts > 0 {
            if pa.maximum_attempts > 0 {
                details.push(format!(
                    "attempt {}/{}",
                    pa.attempt.max(1),
                    pa.maximum_attempts
                ));
            } else {
                details.push(format!("attempt {}", pa.attempt));
            }
        }
        let hb = one_line_preview(&decode_payloads(pa.heartbeat_details), 100);
        if !hb.is_empty() {
            details.push(format!("heartbeat: {hb}"));
        }
        if let Some(failure) = pa.last_failure {
            let err = one_line_preview(&format_failure_chain(Some(failure)), 120);
            details.push(format!("last error: {err}"));
        }
        slot.detail = details.join(" · ");
    }

    let wf_open = ExecStatus::from_proto(info.status).is_open();
    for slot in &mut slots {
        if slot.duration.is_empty()
            && wf_open
            && matches!(slot.state, StepState::Running | StepState::Retrying)
            && let Some(s) = slot.start_ms
        {
            slot.duration = format!(
                "{} elapsed",
                wire::format_duration_ms(now_ms.saturating_sub(s))
            );
        }
    }

    if slots.len() > 128 {
        slots.drain(..slots.len() - 128);
    }
    if events.len() > 200 {
        events.drain(..events.len() - 200);
    }

    let steps = slots
        .into_iter()
        .map(|s| RawStep {
            step: WorkflowStep {
                kind: s.kind.into(),
                name: s.name,
                state: s.state,
                duration: s.duration,
                detail: s.detail,
            },
            child_queue: s.child_queue,
        })
        .collect();

    RawInspection {
        elapsed,
        input,
        task_warning,
        steps,
        events,
        outcome,
    }
}
