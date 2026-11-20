use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body, Frame, Incoming};
use hyper::client::conn::http2 as client_http2;
use hyper::server::conn::http2 as server_http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use temporalio_common::protos::temporal::api::{
    command::v1::command::Attributes as CommandAttrs,
    common::v1::WorkflowExecution,
    enums::v1::TaskQueueKind,
    schedule::v1::{
        BackfillRequest, Schedule, SchedulePatch, schedule_action::Action as ScheduleActionKind,
    },
    workflow::v1::{NewWorkflowExecutionInfo, WorkflowExecutionInfo},
    workflowservice::v1::{
        CountSchedulesRequest, CreateScheduleRequest, DeleteScheduleRequest,
        DescribeNamespaceRequest, DescribeNamespaceResponse, DescribeScheduleRequest,
        DescribeScheduleResponse, DescribeWorkflowExecutionRequest,
        DescribeWorkflowExecutionResponse, GetWorkflowExecutionHistoryRequest,
        ListScheduleMatchingTimesRequest, ListSchedulesRequest, PatchScheduleRequest,
        PollActivityTaskQueueRequest, PollActivityTaskQueueResponse, PollNexusTaskQueueRequest,
        PollWorkflowTaskQueueRequest, PollWorkflowTaskQueueResponse,
        RecordActivityTaskHeartbeatByIdRequest, RecordActivityTaskHeartbeatRequest,
        RecordWorkerHeartbeatRequest, RequestCancelWorkflowExecutionRequest,
        RespondActivityTaskCanceledByIdRequest, RespondActivityTaskCanceledRequest,
        RespondActivityTaskCompletedByIdRequest, RespondActivityTaskCompletedRequest,
        RespondActivityTaskFailedByIdRequest, RespondActivityTaskFailedRequest,
        RespondNexusTaskCompletedRequest, RespondNexusTaskFailedRequest,
        RespondQueryTaskCompletedRequest, RespondWorkflowTaskCompletedRequest,
        RespondWorkflowTaskCompletedResponse, RespondWorkflowTaskFailedRequest,
        ShutdownWorkerRequest, StartWorkflowExecutionRequest, StartWorkflowExecutionResponse,
        UpdateScheduleRequest,
    },
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore, oneshot};

use boreal_common::catalog::{self, Catalog};

use crate::ledger::{self, Coordinator, RemoteTaskKind, TrackedKey};
use crate::remote::RemoteIngress;

const MAX_GRPC_BODY_BYTES: usize = 16 * 1024 * 1024;
const WORKFLOW_SERVICE_PREFIX: &str = "/temporal.api.workflowservice.v1.WorkflowService/";
const MAX_CATCHUP_SECS: i64 = 60;
const MIN_CATCHUP_SECS: i64 = 10;
const MAX_BACKFILL_SECS: i64 = 24 * 60 * 60;
const MAX_SCHEDULE_QUERY_BYTES: usize = 1024;

#[derive(Clone, Debug)]
pub struct WorkerBinding {
    pub owner: String,
    pub task_queue: String,
}

pub struct GrpcBody {
    data: Option<Bytes>,
    trailers: Option<HeaderMap>,
}

impl GrpcBody {
    fn empty() -> Self {
        Self {
            data: None,
            trailers: None,
        }
    }

    fn with_parts(data: Bytes, trailers: Option<HeaderMap>) -> Self {
        Self {
            data: if data.is_empty() { None } else { Some(data) },
            trailers,
        }
    }
}

impl Body for GrpcBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(data) = self.data.take() {
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        if let Some(trailers) = self.trailers.take() {
            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
        }
        Poll::Ready(None)
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none() && self.trailers.is_none()
    }
}

struct UpstreamPool {
    addr: String,
    sender: Mutex<Option<client_http2::SendRequest<GrpcBody>>>,
}

impl UpstreamPool {
    fn new(addr: String) -> Self {
        Self {
            addr,
            sender: Mutex::new(None),
        }
    }

    async fn send(&self, req: Request<GrpcBody>) -> Result<Response<Incoming>, String> {
        let mut sender = {
            let mut guard = self.sender.lock().await;
            if let Some(existing) = guard.as_ref()
                && existing.is_ready()
            {
                existing.clone()
            } else {
                let stream = TcpStream::connect(&self.addr)
                    .await
                    .map_err(|e| format!("connect upstream {}: {e}", self.addr))?;
                let _ = stream.set_nodelay(true);
                let (new_sender, conn) =
                    client_http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                        .await
                        .map_err(|e| format!("upstream h2 handshake: {e}"))?;
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                *guard = Some(new_sender.clone());
                new_sender
            }
        };
        sender
            .ready()
            .await
            .map_err(|e| format!("upstream h2 ready: {e}"))?;
        sender
            .send_request(req)
            .await
            .map_err(|e| format!("upstream h2 request: {e}"))
    }
}

pub async fn serve_worker_listener(
    listener: TcpListener,
    binding: WorkerBinding,
    upstream_addr: String,
    coordinator: Arc<Coordinator>,
    catalog: Arc<Catalog>,
    vault: Arc<crate::vault::Policy>,
) {
    let gate = Arc::new(WorkerGate {
        binding: Arc::new(binding),
        upstream: Arc::new(UpstreamPool::new(upstream_addr)),
        coordinator,
        catalog,
        vault,
        remote: false,
    });
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        tokio::spawn(serve_worker_connection(stream, Arc::clone(&gate)));
    }
}

pub async fn serve_remote_listener(
    listener: TcpListener,
    ingress: RemoteIngress,
    upstream_addr: String,
    coordinator: Arc<Coordinator>,
    catalog: Arc<Catalog>,
    vault: Arc<crate::vault::Policy>,
) {
    let upstream = Arc::new(UpstreamPool::new(upstream_addr));
    let handshakes = Arc::new(Semaphore::new(64));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        };
        let Ok(handshake) = Arc::clone(&handshakes).try_acquire_owned() else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let ingress = ingress.clone();
        let upstream = Arc::clone(&upstream);
        let coordinator = Arc::clone(&coordinator);
        let catalog = Arc::clone(&catalog);
        let vault = Arc::clone(&vault);
        tokio::spawn(async move {
            let accepted = ingress.accept(stream).await;
            drop(handshake);
            if let Some((stream, binding)) = accepted {
                serve_worker_connection(
                    stream,
                    Arc::new(WorkerGate {
                        binding,
                        upstream,
                        coordinator,
                        catalog,
                        vault,
                        remote: true,
                    }),
                )
                .await;
            }
        });
    }
}

struct WorkerGate {
    binding: Arc<WorkerBinding>,
    upstream: Arc<UpstreamPool>,
    coordinator: Arc<Coordinator>,
    catalog: Arc<Catalog>,
    vault: Arc<crate::vault::Policy>,
    remote: bool,
}

async fn serve_worker_connection<IO>(stream: IO, gate: Arc<WorkerGate>)
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = service_fn(move |req: Request<Incoming>| {
        let gate = Arc::clone(&gate);
        async move { Ok::<_, Infallible>(handle_worker_rpc(req, gate).await) }
    });
    let _ = server_http2::Builder::new(TokioExecutor::new())
        .serve_connection(TokioIo::new(stream), svc)
        .await;
}

fn grpc_error(code: u8, message: &str) -> Response<GrpcBody> {
    let mut resp = Response::new(GrpcBody::empty());
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    headers.insert("grpc-accept-encoding", HeaderValue::from_static("identity"));
    headers.insert(
        "grpc-status",
        HeaderValue::from_str(&code.to_string()).unwrap_or_else(|_| HeaderValue::from_static("13")),
    );
    let sanitized: String = message
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .take(200)
        .collect();
    if let Ok(val) = HeaderValue::from_str(&sanitized) {
        headers.insert("grpc-message", val);
    }
    resp
}

fn unpack_unary_grpc(bytes: &[u8]) -> Result<&[u8], (u8, &'static str)> {
    if bytes.is_empty() {
        return Ok(&[]);
    }
    if bytes.len() < 5 {
        return Err((3, "malformed gRPC frame"));
    }
    if bytes[0] != 0 {
        return Err((
            12,
            "unsupported grpc-encoding: compressed frames are not permitted",
        ));
    }
    let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    if bytes.len() != 5 + len {
        return Err((3, "unary gRPC frame length mismatch"));
    }
    Ok(&bytes[5..])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PostTrack {
    None,
    PollWorkflow,
    PollActivity,
    StartWorkflow,
}

struct GatePermit {
    track: PostTrack,
    rewritten: Option<Vec<u8>>,
    admit: Option<(String, String)>,
    finish_remote: Option<(RemoteTaskKind, Vec<u8>)>,
    finish_local: Option<Vec<u8>>,
}

fn permit(track: PostTrack) -> GatePermit {
    GatePermit {
        track,
        rewritten: None,
        admit: None,
        finish_remote: None,
        finish_local: None,
    }
}

enum Admission {
    Open(u64),
    Delegated {
        namespace: String,
        workflow_id: String,
    },
}

#[allow(deprecated)]
async fn authorize_worker_request(
    method: &str,
    payload: &[u8],
    header_ns: Option<&str>,
    gate: &WorkerGate,
) -> Result<GatePermit, (u8, String)> {
    let WorkerGate {
        binding,
        coordinator,
        catalog,
        upstream,
        remote,
        vault,
    } = gate;
    let remote = *remote;
    let vault_worker = vault.contains(&binding.task_queue);
    let vault_enabled = vault.has_vault(&binding.owner);
    let scope_sticky = remote || vault_enabled;
    let deny = |msg: &str| (7u8, msg.to_owned());
    let bad = |msg: &str| (3u8, msg.to_owned());

    if crate::vault::reserved_queue(&binding.task_queue) && !vault_worker {
        return Err(deny("the worker is bound to an unconfigured vault queue"));
    }

    if vault_worker
        && (remote
            || !matches!(
                method,
                "GetSystemInfo"
                    | "DescribeNamespace"
                    | "PollActivityTaskQueue"
                    | "RecordActivityTaskHeartbeat"
                    | "RespondActivityTaskCompleted"
                    | "RespondActivityTaskFailed"
                    | "RespondActivityTaskCanceled"
                    | "RecordWorkerHeartbeat"
                    | "ShutdownWorker"
            ))
    {
        return Err(deny(
            "RPC is not permitted on the central activity-only vault gate",
        ));
    }

    let has_lease = |token: &[u8], kind| {
        if remote {
            coordinator.has_remote_lease(token, &binding.owner, &binding.task_queue, kind)
        } else {
            coordinator.has_local_lease(token, &binding.owner, &binding.task_queue, kind)
        }
    };

    let check_ns = |ns: &str| -> Result<(), (u8, String)> {
        header_matches(header_ns, ns)?;
        if ns == binding.owner {
            Ok(())
        } else {
            Err(deny("worker cross-namespace access denied"))
        }
    };

    let completion = |token: Vec<u8>,
                      kind: RemoteTaskKind,
                      terminal: bool|
     -> Result<GatePermit, (u8, String)> {
        let mut permit = permit(PostTrack::None);
        if remote {
            if !coordinator.has_remote_lease(&token, &binding.owner, &binding.task_queue, kind) {
                // This could also be a retry of an already accepted completion.
                return Err((
                    5,
                    "no outstanding remote task with this token and binding".into(),
                ));
            }
            if terminal {
                permit.finish_remote = Some((kind, token));
            }
        } else if vault_worker {
            if !has_lease(&token, kind) {
                return Err(deny("vault activity token was not delivered by this gate"));
            }
            if terminal {
                permit.finish_local = Some(token);
            }
        } else {
            coordinator.complete_lease(&token);
        }
        Ok(permit)
    };

    if remote
        && matches!(
            method,
            "PollNexusTaskQueue"
                | "RespondNexusTaskCompleted"
                | "RespondNexusTaskFailed"
                | "RespondActivityTaskCompletedById"
                | "RespondActivityTaskFailedById"
                | "RespondActivityTaskCanceledById"
        )
    {
        return Err(deny(
            "RPC cannot be accounted for on the remote worker gate",
        ));
    }

    if vault_enabled
        && matches!(
            method,
            "RecordActivityTaskHeartbeatById"
                | "RespondActivityTaskCompletedById"
                | "RespondActivityTaskFailedById"
                | "RespondActivityTaskCanceledById"
        )
    {
        return Err(deny(
            "vault-enabled namespaces require activity task tokens",
        ));
    }

    match method {
        "GetSystemInfo" => Ok(permit(PostTrack::None)),
        "DescribeNamespace" => {
            let req = DescribeNamespaceRequest::decode(payload)
                .map_err(|_| bad("invalid DescribeNamespaceRequest"))?;
            check_ns(&req.namespace)?;
            Ok(permit(PostTrack::None))
        }
        "PollWorkflowTaskQueue" => {
            let mut req = PollWorkflowTaskQueueRequest::decode(payload)
                .map_err(|_| bad("invalid PollWorkflowTaskQueueRequest"))?;
            check_ns(&req.namespace)?;
            let Some(tq) = req.task_queue.as_mut() else {
                return Err(bad("missing task_queue"));
            };
            if tq.kind() == TaskQueueKind::Sticky {
                if (scope_sticky || !tq.normal_name.is_empty())
                    && tq.normal_name != binding.task_queue
                {
                    return Err(deny("sticky poll normal_name mismatch"));
                }
                if scope_sticky {
                    tq.name = remote_sticky_name(binding, &tq.name);
                }
            } else if tq.name != binding.task_queue {
                return Err(deny("worker task_queue access denied"));
            }
            let mut permit = permit(PostTrack::PollWorkflow);
            if scope_sticky {
                req.worker_control_task_queue.clear();
                permit.rewritten = Some(req.encode_to_vec());
            }
            Ok(permit)
        }
        "PollActivityTaskQueue" => {
            let mut req = PollActivityTaskQueueRequest::decode(payload)
                .map_err(|_| bad("invalid PollActivityTaskQueueRequest"))?;
            check_ns(&req.namespace)?;
            let Some(tq) = req.task_queue.as_ref() else {
                return Err(bad("missing task_queue"));
            };
            if tq.name != binding.task_queue
                || ((remote || vault_worker) && tq.kind() == TaskQueueKind::Sticky)
            {
                return Err(deny("worker activity task_queue access denied"));
            }
            let mut permit = permit(PostTrack::PollActivity);
            if remote || vault_worker {
                req.worker_control_task_queue.clear();
                permit.rewritten = Some(req.encode_to_vec());
            }
            Ok(permit)
        }
        "PollNexusTaskQueue" => {
            let req = PollNexusTaskQueueRequest::decode(payload)
                .map_err(|_| bad("invalid PollNexusTaskQueueRequest"))?;
            check_ns(&req.namespace)?;
            let Some(tq) = req.task_queue.as_ref() else {
                return Err(bad("missing task_queue"));
            };
            if tq.name != binding.task_queue {
                return Err(deny("worker nexus task_queue access denied"));
            }
            Ok(permit(PostTrack::None))
        }
        "RespondWorkflowTaskCompleted" => {
            let mut req = RespondWorkflowTaskCompletedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondWorkflowTaskCompletedRequest"))?;
            check_ns(&req.namespace)?;
            let mut child_keys = Vec::new();
            for cmd in &mut req.commands {
                match &mut cmd.attributes {
                    Some(CommandAttrs::StartChildWorkflowExecutionCommandAttributes(attr)) => {
                        if attr
                            .task_queue
                            .as_ref()
                            .is_some_and(|q| crate::vault::reserved_queue(&q.name))
                        {
                            return Err(deny("vault queues do not run workflows"));
                        }
                        if !attr.namespace.is_empty() && attr.namespace != binding.owner {
                            return Err(deny("cross-namespace child workflow denied"));
                        }
                        if attr.workflow_id.starts_with("temporal-sys-") {
                            return Err(deny("reserved temporal-sys- child workflow ID denied"));
                        }
                        if !attr.workflow_id.is_empty() {
                            child_keys.push(TrackedKey {
                                namespace: binding.owner.clone(),
                                workflow_id: attr.workflow_id.clone(),
                                run_id: None,
                            });
                        }
                    }
                    Some(CommandAttrs::SignalExternalWorkflowExecutionCommandAttributes(attr))
                        if !attr.namespace.is_empty() && attr.namespace != binding.owner =>
                    {
                        return Err(deny("cross-namespace external signal denied"));
                    }
                    Some(
                        CommandAttrs::RequestCancelExternalWorkflowExecutionCommandAttributes(attr),
                    ) if !attr.namespace.is_empty() && attr.namespace != binding.owner => {
                        return Err(deny("cross-namespace external cancel denied"));
                    }
                    Some(CommandAttrs::ContinueAsNewWorkflowExecutionCommandAttributes(attr))
                        if attr
                            .task_queue
                            .as_ref()
                            .is_some_and(|q| crate::vault::reserved_queue(&q.name)) =>
                    {
                        return Err(deny("vault queues do not run workflows"));
                    }
                    Some(CommandAttrs::ScheduleActivityTaskCommandAttributes(attr)) => {
                        if let Some(queue) = attr
                            .task_queue
                            .as_ref()
                            .filter(|q| crate::vault::reserved_queue(&q.name))
                        {
                            let operation = attr
                                .activity_type
                                .as_ref()
                                .map_or("", |ty| ty.name.as_str());
                            if queue.kind() == TaskQueueKind::Sticky
                                || !vault.allows(
                                    &binding.owner,
                                    &binding.task_queue,
                                    &queue.name,
                                    operation,
                                )
                                || !has_lease(&req.task_token, RemoteTaskKind::Workflow)
                            {
                                return Err(deny(
                                    "vault activity is not permitted for this workflow task",
                                ));
                            }
                            attr.request_eager_execution = false;
                        } else if remote {
                            attr.request_eager_execution = false;
                        }
                    }
                    _ => {}
                }
            }
            let mut permit = if remote {
                completion(
                    req.task_token.clone(),
                    RemoteTaskKind::Workflow,
                    !req.intermediate_page,
                )?
            } else {
                permit(PostTrack::None)
            };
            if scope_sticky {
                if let Some(tq) = req
                    .sticky_attributes
                    .as_mut()
                    .and_then(|attr| attr.worker_task_queue.as_mut())
                {
                    if tq.kind() != TaskQueueKind::Sticky || tq.normal_name != binding.task_queue {
                        return Err(deny("sticky completion task_queue mismatch"));
                    }
                    tq.name = remote_sticky_name(binding, &tq.name);
                }
                req.return_new_workflow_task = false;
                req.worker_control_task_queue.clear();
                permit.rewritten = Some(req.encode_to_vec());
            }
            if !child_keys.is_empty() {
                let tracked = coordinator.track_runs(child_keys);
                if remote {
                    tracked.map_err(|e| (13, e.detail()))?;
                }
            }
            if !remote && scope_sticky && !req.intermediate_page {
                permit.finish_local = Some(req.task_token);
            } else if !remote && !scope_sticky {
                coordinator.complete_lease(&req.task_token);
            }
            Ok(permit)
        }
        "RespondWorkflowTaskFailed" => {
            let req = RespondWorkflowTaskFailedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondWorkflowTaskFailedRequest"))?;
            check_ns(&req.namespace)?;
            completion(req.task_token, RemoteTaskKind::Workflow, true)
        }
        "RespondQueryTaskCompleted" => {
            let req = RespondQueryTaskCompletedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondQueryTaskCompletedRequest"))?;
            check_ns(&req.namespace)?;
            if remote {
                completion(req.task_token, RemoteTaskKind::Query, true)
            } else {
                Ok(permit(PostTrack::None))
            }
        }
        "RecordActivityTaskHeartbeat" => {
            let req = RecordActivityTaskHeartbeatRequest::decode(payload)
                .map_err(|_| bad("invalid RecordActivityTaskHeartbeatRequest"))?;
            check_ns(&req.namespace)?;
            if vault_worker && !has_lease(&req.task_token, RemoteTaskKind::Activity) {
                return Err(deny("vault activity token was not delivered by this gate"));
            }
            Ok(permit(PostTrack::None))
        }
        "RecordActivityTaskHeartbeatById" => {
            let req = RecordActivityTaskHeartbeatByIdRequest::decode(payload)
                .map_err(|_| bad("invalid RecordActivityTaskHeartbeatByIdRequest"))?;
            check_ns(&req.namespace)?;
            Ok(permit(PostTrack::None))
        }
        "RespondActivityTaskCompleted" => {
            let req = RespondActivityTaskCompletedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskCompletedRequest"))?;
            check_ns(&req.namespace)?;
            completion(req.task_token, RemoteTaskKind::Activity, true)
        }
        "RespondActivityTaskCompletedById" => {
            let req = RespondActivityTaskCompletedByIdRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskCompletedByIdRequest"))?;
            check_ns(&req.namespace)?;
            Ok(permit(PostTrack::None))
        }
        "RespondActivityTaskFailed" => {
            let req = RespondActivityTaskFailedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskFailedRequest"))?;
            check_ns(&req.namespace)?;
            completion(req.task_token, RemoteTaskKind::Activity, true)
        }
        "RespondActivityTaskFailedById" => {
            let req = RespondActivityTaskFailedByIdRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskFailedByIdRequest"))?;
            check_ns(&req.namespace)?;
            Ok(permit(PostTrack::None))
        }
        "RespondActivityTaskCanceled" => {
            let req = RespondActivityTaskCanceledRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskCanceledRequest"))?;
            check_ns(&req.namespace)?;
            completion(req.task_token, RemoteTaskKind::Activity, true)
        }
        "RespondActivityTaskCanceledById" => {
            let req = RespondActivityTaskCanceledByIdRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskCanceledByIdRequest"))?;
            check_ns(&req.namespace)?;
            Ok(permit(PostTrack::None))
        }
        "RespondNexusTaskCompleted" => {
            let req = RespondNexusTaskCompletedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondNexusTaskCompletedRequest"))?;
            check_ns(&req.namespace)?;
            Ok(permit(PostTrack::None))
        }
        "RespondNexusTaskFailed" => {
            let req = RespondNexusTaskFailedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondNexusTaskFailedRequest"))?;
            check_ns(&req.namespace)?;
            Ok(permit(PostTrack::None))
        }
        "GetWorkflowExecutionHistory" => {
            let req = GetWorkflowExecutionHistoryRequest::decode(payload)
                .map_err(|_| bad("invalid GetWorkflowExecutionHistoryRequest"))?;
            header_matches(header_ns, &req.namespace)?;
            if req.namespace == binding.owner {
                return Ok(permit(PostTrack::None));
            }
            let execution = req
                .execution
                .as_ref()
                .ok_or_else(|| deny("worker history access denied"))?;
            if execution.workflow_id.is_empty() {
                return Err(deny("worker history access denied"));
            }
            let info = describe_execution(
                upstream,
                &req.namespace,
                &execution.workflow_id,
                &execution.run_id,
            )
            .await?;
            authorize_described(
                catalog,
                &binding.owner,
                &req.namespace,
                &info,
                &execution.run_id,
                false,
            )?;
            Ok(permit(PostTrack::None))
        }
        "StartWorkflowExecution" => {
            let mut req = StartWorkflowExecutionRequest::decode(payload)
                .map_err(|_| bad("invalid StartWorkflowExecutionRequest"))?;
            header_matches(header_ns, &req.namespace)?;
            authorize_start(catalog, &binding.owner, &req)?;
            let mut permit = permit(PostTrack::StartWorkflow);
            if remote || vault.has_vault(&req.namespace) {
                req.request_eager_execution = false;
                permit.rewritten = Some(req.encode_to_vec());
            }
            permit.admit = Some((req.namespace, req.workflow_id));
            Ok(permit)
        }
        "RequestCancelWorkflowExecution" => {
            let req = RequestCancelWorkflowExecutionRequest::decode(payload)
                .map_err(|_| bad("invalid RequestCancelWorkflowExecutionRequest"))?;
            header_matches(header_ns, &req.namespace)?;
            let execution = req
                .workflow_execution
                .as_ref()
                .ok_or_else(|| deny("worker cancel access denied"))?;
            if execution.workflow_id.is_empty() || execution.run_id.is_empty() {
                return Err(deny("worker cancel access denied"));
            }
            let info = describe_execution(
                upstream,
                &req.namespace,
                &execution.workflow_id,
                &execution.run_id,
            )
            .await?;
            authorize_described(
                catalog,
                &binding.owner,
                &req.namespace,
                &info,
                &execution.run_id,
                true,
            )?;
            Ok(permit(PostTrack::None))
        }
        "ShutdownWorker" => {
            let mut req = ShutdownWorkerRequest::decode(payload)
                .map_err(|_| bad("invalid ShutdownWorkerRequest"))?;
            check_ns(&req.namespace)?;
            if !req.task_queue.is_empty() && req.task_queue != binding.task_queue {
                return Err(deny("worker shutdown task_queue mismatch"));
            }
            let mut permit = permit(PostTrack::None);
            if scope_sticky {
                req.task_queue.clone_from(&binding.task_queue);
                if !req.sticky_task_queue.is_empty() {
                    req.sticky_task_queue = remote_sticky_name(binding, &req.sticky_task_queue);
                }
                permit.rewritten = Some(req.encode_to_vec());
            }
            Ok(permit)
        }
        "RecordWorkerHeartbeat" => {
            let req = RecordWorkerHeartbeatRequest::decode(payload)
                .map_err(|_| bad("invalid RecordWorkerHeartbeatRequest"))?;
            check_ns(&req.namespace)?;
            Ok(permit(PostTrack::None))
        }
        "CreateSchedule" => authorize_create_schedule(payload, header_ns, binding, catalog),
        "UpdateSchedule" => {
            authorize_update_schedule(payload, header_ns, binding, catalog, upstream).await
        }
        "PatchSchedule" => {
            authorize_patch_schedule(payload, header_ns, binding, catalog, upstream).await
        }
        "DeleteSchedule" => {
            authorize_delete_schedule(payload, header_ns, binding, catalog, upstream).await
        }
        "DescribeSchedule" => {
            authorize_describe_schedule(payload, header_ns, binding, catalog, upstream).await
        }
        "ListSchedules" => authorize_list_schedules(payload, header_ns, binding),
        "CountSchedules" => authorize_count_schedules(payload, header_ns, binding),
        "ListScheduleMatchingTimes" => authorize_matching_times(payload, header_ns, binding),
        _ => Err(deny("RPC method is not permitted on Boreal worker gate")),
    }
}

fn remote_sticky_name(binding: &WorkerBinding, name: &str) -> String {
    // The supplied normal_name alone is not proof of sticky queue ownership.
    // Scope both registration and lookup to the authenticated worker binding.
    let data = format!("{}\0{}\0{name}", binding.owner, binding.task_queue);
    format!(
        "boreal-remote-sticky-{}",
        ledger::digest_hex(data.as_bytes())
    )
}

fn header_matches(header_ns: Option<&str>, namespace: &str) -> Result<(), (u8, String)> {
    if let Some(header) = header_ns
        && header != namespace
    {
        return Err((7, "temporal-namespace header mismatch".to_owned()));
    }
    Ok(())
}

fn schedule_denied() -> (u8, String) {
    (7, "worker schedule access denied".to_owned())
}

fn require_schedule_id(id: &str) -> Result<(), (u8, String)> {
    catalog::validate_workflow_id(id).map_err(|_| schedule_denied())
}

fn require_own_namespace(binding: &WorkerBinding, namespace: &str) -> Result<(), (u8, String)> {
    if namespace == binding.owner {
        Ok(())
    } else {
        Err(schedule_denied())
    }
}

fn require_query(query: &str) -> Result<(), (u8, String)> {
    if query.len() > MAX_SCHEDULE_QUERY_BYTES || query.bytes().any(|b| b.is_ascii_control()) {
        Err((3, "invalid schedule query".to_owned()))
    } else {
        Ok(())
    }
}

fn schedule_is_paused(schedule: &Schedule) -> bool {
    schedule.state.as_ref().is_some_and(|state| state.paused)
}

fn patch_starts_work(patch: &SchedulePatch) -> bool {
    patch.trigger_immediately.is_some()
        || !patch.unpause.is_empty()
        || !patch.backfill_request.is_empty()
}

fn backfill_allowed(req: &BackfillRequest) -> bool {
    let (Some(start), Some(end)) = (req.start_time.as_ref(), req.end_time.as_ref()) else {
        return false;
    };
    if start.seconds < 0 || end.seconds < 0 || start.nanos < 0 || end.nanos < 0 {
        return false;
    }
    if end.seconds < start.seconds || (end.seconds == start.seconds && end.nanos < start.nanos) {
        return false;
    }
    let span = end.seconds - start.seconds;
    span < MAX_BACKFILL_SECS || (span == MAX_BACKFILL_SECS && end.nanos <= start.nanos)
}

fn patch_allowed(patch: &SchedulePatch) -> bool {
    patch.time_skipping_config.is_none() && patch.backfill_request.iter().all(backfill_allowed)
}

fn catchup_in_range(seconds: i64, nanos: i32) -> bool {
    nanos == 0 && (MIN_CATCHUP_SECS..=MAX_CATCHUP_SECS).contains(&seconds)
}

fn clamp_catchup(schedule: &mut Schedule) {
    let policies = schedule.policies.get_or_insert_with(Default::default);
    let in_range = policies
        .catchup_window
        .as_ref()
        .is_some_and(|window| catchup_in_range(window.seconds, window.nanos));
    if in_range {
        return;
    }
    // Temporal's default catchup is a year. An updater unpause would replay it.
    let window = policies.catchup_window.get_or_insert_with(Default::default);
    window.seconds = MAX_CATCHUP_SECS;
    window.nanos = 0;
}

fn authorize_schedule_action(
    catalog: &Catalog,
    caller: &str,
    namespace: &str,
    info: &NewWorkflowExecutionInfo,
) -> Result<(), (u8, String)> {
    if !info.cron_schedule.is_empty() {
        return Err(schedule_denied());
    }
    if !info.workflow_id.is_empty() {
        require_schedule_id(&info.workflow_id)?;
    }
    let Some(task_queue) = info.task_queue.as_ref() else {
        return Err(schedule_denied());
    };
    if task_queue.kind() == TaskQueueKind::Sticky || task_queue.name.is_empty() {
        return Err(schedule_denied());
    }
    let prefix = format!("{namespace}-");
    let Some(worker) = task_queue.name.strip_prefix(&prefix) else {
        return Err(schedule_denied());
    };
    if worker.is_empty() {
        return Err(schedule_denied());
    }
    let Ok((_, pkg)) = catalog.worker_and_package(namespace, worker) else {
        return Err(schedule_denied());
    };
    let workflow_type = info
        .workflow_type
        .as_ref()
        .map(|ty| ty.name.as_str())
        .unwrap_or("");
    if pkg.workflow_by_type(workflow_type).is_none()
        || !catalog.can_write(caller, namespace, worker)
    {
        return Err(schedule_denied());
    }
    Ok(())
}

fn authorize_schedule_body(
    catalog: &Catalog,
    caller: &str,
    namespace: &str,
    schedule_id: &str,
    schedule: &Schedule,
) -> Result<(), (u8, String)> {
    require_schedule_id(schedule_id)?;
    let Some(action) = schedule.action.as_ref() else {
        return Err(schedule_denied());
    };
    let Some(ScheduleActionKind::StartWorkflow(info)) = action.action.as_ref() else {
        return Err(schedule_denied());
    };
    authorize_schedule_action(catalog, caller, namespace, info)
}

fn admission_workflow_id(schedule_id: &str, schedule: &Schedule) -> String {
    schedule
        .action
        .as_ref()
        .and_then(|action| match action.action.as_ref() {
            Some(ScheduleActionKind::StartWorkflow(info)) if !info.workflow_id.is_empty() => {
                Some(info.workflow_id.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| schedule_id.to_owned())
}

fn create_starts_work(schedule: &Schedule, patch: Option<&SchedulePatch>) -> bool {
    !schedule_is_paused(schedule) || patch.is_some_and(patch_starts_work)
}

fn authorize_create_schedule(
    payload: &[u8],
    header_ns: Option<&str>,
    binding: &WorkerBinding,
    catalog: &Catalog,
) -> Result<GatePermit, (u8, String)> {
    let mut req = CreateScheduleRequest::decode(payload)
        .map_err(|_| (3u8, "invalid CreateScheduleRequest".to_owned()))?;
    header_matches(header_ns, &req.namespace)?;
    let Some(schedule) = req.schedule.as_mut() else {
        return Err(schedule_denied());
    };
    authorize_schedule_body(
        catalog,
        &binding.owner,
        &req.namespace,
        &req.schedule_id,
        schedule,
    )?;
    if let Some(patch) = req.initial_patch.as_ref()
        && !patch_allowed(patch)
    {
        return Err(schedule_denied());
    }
    let starts = create_starts_work(schedule, req.initial_patch.as_ref());
    clamp_catchup(schedule);
    let admit = starts.then_some((
        req.namespace.clone(),
        admission_workflow_id(&req.schedule_id, schedule),
    ));
    Ok(GatePermit {
        track: PostTrack::None,
        rewritten: Some(req.encode_to_vec()),
        admit,
        finish_remote: None,
        finish_local: None,
    })
}

fn authorize_list_schedules(
    payload: &[u8],
    header_ns: Option<&str>,
    binding: &WorkerBinding,
) -> Result<GatePermit, (u8, String)> {
    let req = ListSchedulesRequest::decode(payload)
        .map_err(|_| (3u8, "invalid ListSchedulesRequest".to_owned()))?;
    header_matches(header_ns, &req.namespace)?;
    require_own_namespace(binding, &req.namespace)?;
    require_query(&req.query)?;
    Ok(permit(PostTrack::None))
}

fn authorize_count_schedules(
    payload: &[u8],
    header_ns: Option<&str>,
    binding: &WorkerBinding,
) -> Result<GatePermit, (u8, String)> {
    let req = CountSchedulesRequest::decode(payload)
        .map_err(|_| (3u8, "invalid CountSchedulesRequest".to_owned()))?;
    header_matches(header_ns, &req.namespace)?;
    require_own_namespace(binding, &req.namespace)?;
    require_query(&req.query)?;
    Ok(permit(PostTrack::None))
}

fn authorize_matching_times(
    payload: &[u8],
    header_ns: Option<&str>,
    binding: &WorkerBinding,
) -> Result<GatePermit, (u8, String)> {
    let req = ListScheduleMatchingTimesRequest::decode(payload)
        .map_err(|_| (3u8, "invalid ListScheduleMatchingTimesRequest".to_owned()))?;
    header_matches(header_ns, &req.namespace)?;
    require_own_namespace(binding, &req.namespace)?;
    require_schedule_id(&req.schedule_id)?;
    Ok(permit(PostTrack::None))
}

async fn load_schedule(
    upstream: &UpstreamPool,
    namespace: &str,
    schedule_id: &str,
) -> Result<Schedule, (u8, String)> {
    let req = DescribeScheduleRequest {
        namespace: namespace.to_owned(),
        schedule_id: schedule_id.to_owned(),
    };
    let frame = pack_unary_grpc(&req.encode_to_vec());
    let path = format!("{WORKFLOW_SERVICE_PREFIX}DescribeSchedule");
    let uri = Uri::builder()
        .scheme("http")
        .authority(upstream.addr.as_str())
        .path_and_query(path)
        .build()
        .map_err(|_| (13u8, "failed to construct describe URI".to_owned()))?;
    let mut out = Request::new(GrpcBody::with_parts(frame, None));
    *out.method_mut() = http::Method::POST;
    *out.uri_mut() = uri;
    *out.version_mut() = http::Version::HTTP_2;
    out.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    out.headers_mut()
        .insert("te", HeaderValue::from_static("trailers"));

    let resp = upstream.send(out).await.map_err(|err| (14u8, err))?;
    let (parts, body) = resp.into_parts();
    let buffered = Limited::new(body, MAX_GRPC_BODY_BYTES)
        .collect()
        .await
        .map_err(|_| (14u8, "failed to collect describe response".to_owned()))?;
    let trailers = buffered.trailers().cloned();
    if resp_Parts_has_grpc_error(&parts.headers, trailers.as_ref()) {
        return Err(grpc_status_error(&parts.headers, trailers.as_ref()));
    }
    let bytes = buffered.to_bytes();
    let payload = unpack_unary_grpc(&bytes).map_err(|(code, msg)| (code, msg.to_owned()))?;
    let decoded = DescribeScheduleResponse::decode(payload)
        .map_err(|_| (13u8, "invalid DescribeScheduleResponse".to_owned()))?;
    decoded.schedule.ok_or_else(|| schedule_denied())
}

async fn authorize_existing(
    upstream: &UpstreamPool,
    catalog: &Catalog,
    caller: &str,
    namespace: &str,
    schedule_id: &str,
) -> Result<Schedule, (u8, String)> {
    require_schedule_id(schedule_id)?;
    let schedule = load_schedule(upstream, namespace, schedule_id).await?;
    authorize_schedule_body(catalog, caller, namespace, schedule_id, &schedule)?;
    Ok(schedule)
}

async fn authorize_update_schedule(
    payload: &[u8],
    header_ns: Option<&str>,
    binding: &WorkerBinding,
    catalog: &Catalog,
    upstream: &UpstreamPool,
) -> Result<GatePermit, (u8, String)> {
    let mut req = UpdateScheduleRequest::decode(payload)
        .map_err(|_| (3u8, "invalid UpdateScheduleRequest".to_owned()))?;
    header_matches(header_ns, &req.namespace)?;
    authorize_existing(
        upstream,
        catalog,
        &binding.owner,
        &req.namespace,
        &req.schedule_id,
    )
    .await?;
    let Some(schedule) = req.schedule.as_mut() else {
        return Err(schedule_denied());
    };
    authorize_schedule_body(
        catalog,
        &binding.owner,
        &req.namespace,
        &req.schedule_id,
        schedule,
    )?;
    let starts = !schedule_is_paused(schedule);
    clamp_catchup(schedule);
    let admit = starts.then_some((
        req.namespace.clone(),
        admission_workflow_id(&req.schedule_id, schedule),
    ));
    Ok(GatePermit {
        track: PostTrack::None,
        rewritten: Some(req.encode_to_vec()),
        admit,
        finish_remote: None,
        finish_local: None,
    })
}

async fn authorize_patch_schedule(
    payload: &[u8],
    header_ns: Option<&str>,
    binding: &WorkerBinding,
    catalog: &Catalog,
    upstream: &UpstreamPool,
) -> Result<GatePermit, (u8, String)> {
    let req = PatchScheduleRequest::decode(payload)
        .map_err(|_| (3u8, "invalid PatchScheduleRequest".to_owned()))?;
    header_matches(header_ns, &req.namespace)?;
    let existing = authorize_existing(
        upstream,
        catalog,
        &binding.owner,
        &req.namespace,
        &req.schedule_id,
    )
    .await?;
    let Some(patch) = req.patch.as_ref() else {
        return Err(schedule_denied());
    };
    if !patch_allowed(patch) {
        return Err(schedule_denied());
    }
    let admit = patch_starts_work(patch).then(|| {
        (
            req.namespace.clone(),
            admission_workflow_id(&req.schedule_id, &existing),
        )
    });
    Ok(GatePermit {
        track: PostTrack::None,
        rewritten: None,
        admit,
        finish_remote: None,
        finish_local: None,
    })
}

async fn authorize_delete_schedule(
    payload: &[u8],
    header_ns: Option<&str>,
    binding: &WorkerBinding,
    catalog: &Catalog,
    upstream: &UpstreamPool,
) -> Result<GatePermit, (u8, String)> {
    let req = DeleteScheduleRequest::decode(payload)
        .map_err(|_| (3u8, "invalid DeleteScheduleRequest".to_owned()))?;
    header_matches(header_ns, &req.namespace)?;
    authorize_existing(
        upstream,
        catalog,
        &binding.owner,
        &req.namespace,
        &req.schedule_id,
    )
    .await?;
    Ok(permit(PostTrack::None))
}

async fn authorize_describe_schedule(
    payload: &[u8],
    header_ns: Option<&str>,
    binding: &WorkerBinding,
    catalog: &Catalog,
    upstream: &UpstreamPool,
) -> Result<GatePermit, (u8, String)> {
    let req = DescribeScheduleRequest::decode(payload)
        .map_err(|_| (3u8, "invalid DescribeScheduleRequest".to_owned()))?;
    header_matches(header_ns, &req.namespace)?;
    require_schedule_id(&req.schedule_id)?;
    if req.namespace == binding.owner {
        return Ok(permit(PostTrack::None));
    }
    let schedule = load_schedule(upstream, &req.namespace, &req.schedule_id).await?;
    let Some(action) = schedule.action.as_ref() else {
        return Err(schedule_denied());
    };
    let Some(ScheduleActionKind::StartWorkflow(info)) = action.action.as_ref() else {
        return Err(schedule_denied());
    };
    let Some(task_queue) = info.task_queue.as_ref() else {
        return Err(schedule_denied());
    };
    let prefix = format!("{}-", req.namespace);
    let Some(worker) = task_queue.name.strip_prefix(&prefix) else {
        return Err(schedule_denied());
    };
    let Ok((_, pkg)) = catalog.worker_and_package(&req.namespace, worker) else {
        return Err(schedule_denied());
    };
    let workflow_type = info
        .workflow_type
        .as_ref()
        .map(|ty| ty.name.as_str())
        .unwrap_or("");
    if pkg.workflow_by_type(workflow_type).is_none()
        || !catalog.can_read(&binding.owner, &req.namespace, worker)
    {
        return Err(schedule_denied());
    }
    Ok(permit(PostTrack::None))
}

fn authorize_start(
    catalog: &Catalog,
    caller: &str,
    req: &StartWorkflowExecutionRequest,
) -> Result<(), (u8, String)> {
    let deny = |msg: &str| (7u8, msg.to_owned());
    if catalog::validate_workflow_id(&req.workflow_id).is_err() {
        return Err(deny("worker start denied"));
    }
    let Some(task_queue) = req.task_queue.as_ref() else {
        return Err(deny("worker start denied"));
    };
    if task_queue.kind() == TaskQueueKind::Sticky || task_queue.name.is_empty() {
        return Err(deny("worker start denied"));
    }
    let prefix = format!("{}-", req.namespace);
    let Some(worker) = task_queue.name.strip_prefix(&prefix) else {
        return Err(deny("worker start denied"));
    };
    if worker.is_empty() {
        return Err(deny("worker start denied"));
    }
    let Ok((_, pkg)) = catalog.worker_and_package(&req.namespace, worker) else {
        return Err(deny("worker start denied"));
    };
    let workflow_type = req
        .workflow_type
        .as_ref()
        .map(|ty| ty.name.as_str())
        .unwrap_or("");
    if pkg.workflow_by_type(workflow_type).is_none()
        || !catalog.can_write(caller, &req.namespace, worker)
    {
        return Err(deny("worker start denied"));
    }
    Ok(())
}

fn authorize_described(
    catalog: &Catalog,
    caller: &str,
    namespace: &str,
    info: &WorkflowExecutionInfo,
    pinned_run_id: &str,
    write: bool,
) -> Result<(), (u8, String)> {
    let deny = || (7u8, "worker execution access denied".to_owned());
    let run_id = info
        .execution
        .as_ref()
        .map(|exec| exec.run_id.as_str())
        .unwrap_or("");
    if run_id.is_empty() || (!pinned_run_id.is_empty() && pinned_run_id != run_id) {
        return Err(deny());
    }
    let workflow_type = info
        .r#type
        .as_ref()
        .map(|ty| ty.name.as_str())
        .unwrap_or("");
    let Ok(verified) = catalog.verify_execution(namespace, &info.task_queue, workflow_type, run_id)
    else {
        return Err(deny());
    };
    let allowed = if write {
        catalog.can_write(caller, namespace, verified.worker.name())
    } else {
        catalog.can_read(caller, namespace, verified.worker.name())
    };
    if !allowed {
        return Err(deny());
    }
    Ok(())
}

fn admit_schedule(
    coordinator: &Coordinator,
    namespace: &str,
    workflow_id: &str,
) -> Result<Admission, (u8, String)> {
    match coordinator.begin_start(namespace, workflow_id) {
        Ok(seq) => Ok(Admission::Open(seq)),
        Err(ledger::Error::AdmissionClosed) => Err((9, ledger::Error::AdmissionClosed.detail())),
        Err(err) => Err((13, err.detail())),
    }
}

fn admit_worker_start(
    coordinator: &Coordinator,
    caller: &str,
    namespace: &str,
    workflow_id: &str,
) -> Result<Admission, (u8, String)> {
    match coordinator.begin_start(namespace, workflow_id) {
        Ok(seq) => Ok(Admission::Open(seq)),
        Err(ledger::Error::AdmissionClosed) if coordinator.namespace_has_inflight(caller) => {
            coordinator
                .track_runs([TrackedKey {
                    namespace: namespace.to_owned(),
                    workflow_id: workflow_id.to_owned(),
                    run_id: None,
                }])
                .map_err(|err| (13u8, err.detail()))?;
            Ok(Admission::Delegated {
                namespace: namespace.to_owned(),
                workflow_id: workflow_id.to_owned(),
            })
        }
        Err(ledger::Error::AdmissionClosed) => Err((9, ledger::Error::AdmissionClosed.detail())),
        Err(err) => Err((13, err.detail())),
    }
}

fn finish_admission(coordinator: &Coordinator, admission: Admission, run_id: Option<String>) {
    match admission {
        Admission::Open(seq) => {
            let _ = coordinator.finish_start(seq, run_id);
        }
        Admission::Delegated {
            namespace,
            workflow_id,
        } => {
            if let Some(run_id) = run_id.filter(|id| !id.is_empty()) {
                let _ = coordinator.track_runs([TrackedKey {
                    namespace,
                    workflow_id,
                    run_id: Some(run_id),
                }]);
            }
        }
    }
}

async fn describe_execution(
    upstream: &UpstreamPool,
    namespace: &str,
    workflow_id: &str,
    run_id: &str,
) -> Result<WorkflowExecutionInfo, (u8, String)> {
    let req = DescribeWorkflowExecutionRequest {
        namespace: namespace.to_owned(),
        execution: Some(WorkflowExecution {
            workflow_id: workflow_id.to_owned(),
            run_id: run_id.to_owned(),
        }),
    };
    let frame = pack_unary_grpc(&req.encode_to_vec());
    let path = format!("{WORKFLOW_SERVICE_PREFIX}DescribeWorkflowExecution");
    let uri = Uri::builder()
        .scheme("http")
        .authority(upstream.addr.as_str())
        .path_and_query(path)
        .build()
        .map_err(|_| (13u8, "failed to construct describe URI".to_owned()))?;
    let mut out = Request::new(GrpcBody::with_parts(frame, None));
    *out.method_mut() = http::Method::POST;
    *out.uri_mut() = uri;
    *out.version_mut() = http::Version::HTTP_2;
    out.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    out.headers_mut()
        .insert("te", HeaderValue::from_static("trailers"));

    let resp = upstream.send(out).await.map_err(|err| (14u8, err))?;
    let (parts, body) = resp.into_parts();
    let buffered = Limited::new(body, MAX_GRPC_BODY_BYTES)
        .collect()
        .await
        .map_err(|_| (14u8, "failed to collect describe response".to_owned()))?;
    let trailers = buffered.trailers().cloned();
    if resp_Parts_has_grpc_error(&parts.headers, trailers.as_ref()) {
        return Err(grpc_status_error(&parts.headers, trailers.as_ref()));
    }
    let bytes = buffered.to_bytes();
    let payload = unpack_unary_grpc(&bytes).map_err(|(code, msg)| (code, msg.to_owned()))?;
    let decoded = DescribeWorkflowExecutionResponse::decode(payload)
        .map_err(|_| (13u8, "invalid DescribeWorkflowExecutionResponse".to_owned()))?;
    decoded
        .workflow_execution_info
        .ok_or_else(|| (13u8, "missing workflow_execution_info".to_owned()))
}

fn pack_unary_grpc(encoded: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(5 + encoded.len());
    out.push(0);
    out.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
    out.extend_from_slice(encoded);
    Bytes::from(out)
}

fn grpc_status_error(headers: &HeaderMap, trailers: Option<&HeaderMap>) -> (u8, String) {
    let status = trailers
        .and_then(|map| map.get("grpc-status"))
        .or_else(|| headers.get("grpc-status"));
    let code = status
        .and_then(|val| val.to_str().ok())
        .and_then(|text| text.parse().ok())
        .unwrap_or(13);
    let message = trailers
        .and_then(|map| map.get("grpc-message"))
        .or_else(|| headers.get("grpc-message"))
        .and_then(|val| val.to_str().ok())
        .unwrap_or("upstream gRPC request failed");
    (code, message.to_owned())
}

async fn handle_worker_rpc(req: Request<Incoming>, gate: Arc<WorkerGate>) -> Response<GrpcBody> {
    if req.method() != http::Method::POST {
        return grpc_error(3, "gRPC requires HTTP POST");
    }
    let path = req.uri().path().to_owned();
    let Some(method) = path.strip_prefix(WORKFLOW_SERVICE_PREFIX) else {
        return grpc_error(7, "service is not permitted on Boreal worker gate");
    };
    if let Some(enc_hdr) = req.headers().get("grpc-encoding") {
        let is_identity = enc_hdr
            .to_str()
            .is_ok_and(|s| s.trim().eq_ignore_ascii_case("identity"));
        if !is_identity {
            return grpc_error(12, "unsupported grpc-encoding: only identity is supported");
        }
    }
    let header_ns = match req.headers().get("temporal-namespace") {
        Some(value) => match value.to_str() {
            Ok(text) => Some(text.to_owned()),
            Err(_) => return grpc_error(3, "invalid temporal-namespace header"),
        },
        None => None,
    };

    let (parts, incoming) = req.into_parts();
    let Ok(collected) = Limited::new(incoming, MAX_GRPC_BODY_BYTES).collect().await else {
        return grpc_error(3, "failed to read gRPC request body");
    };
    let req_bytes = collected.to_bytes();
    let payload = match unpack_unary_grpc(&req_bytes) {
        Ok(slice) => slice,
        Err((code, msg)) => return grpc_error(code, msg),
    };

    let permit = match authorize_worker_request(method, payload, header_ns.as_deref(), &gate).await
    {
        Ok(permit) => permit,
        Err((code, msg)) => return grpc_error(code, &msg),
    };

    let forward_bytes = match permit.rewritten.as_deref() {
        Some(encoded) => pack_unary_grpc(encoded),
        None => req_bytes.clone(),
    };

    let Ok(uri) = Uri::builder()
        .scheme("http")
        .authority(gate.upstream.addr.as_str())
        .path_and_query(path.as_str())
        .build()
    else {
        return grpc_error(13, "failed to construct upstream URI");
    };

    let mut out_req = Request::new(GrpcBody::with_parts(forward_bytes, None));
    *out_req.method_mut() = http::Method::POST;
    *out_req.uri_mut() = uri;
    *out_req.version_mut() = http::Version::HTTP_2;
    for (name, val) in &parts.headers {
        if name == http::header::HOST
            || name == "grpc-accept-encoding"
            || (gate.remote && name == "temporal-namespace")
            || (permit.rewritten.is_some() && name == http::header::CONTENT_LENGTH)
        {
            continue;
        }
        out_req.headers_mut().append(name.clone(), val.clone());
    }
    if gate.remote
        && let Some(value) = parts.headers.get("temporal-namespace")
    {
        out_req
            .headers_mut()
            .insert("temporal-namespace", value.clone());
    }

    if gate.remote {
        // A downstream RST_STREAM must not drop the pickup guard while
        // Temporal is still servicing the RPC. Finish independently of the
        // connection, but do not grant work if delivery is already impossible.
        let (reply, result) = oneshot::channel();
        tokio::spawn(async move {
            let response = forward_worker_rpc(out_req, permit, &gate, Some(&reply)).await;
            let _ = reply.send(response);
        });
        result
            .await
            .unwrap_or_else(|_| grpc_error(14, "remote worker RPC interrupted"))
    } else {
        forward_worker_rpc(out_req, permit, &gate, None).await
    }
}

async fn forward_worker_rpc(
    out_req: Request<GrpcBody>,
    permit: GatePermit,
    gate: &WorkerGate,
    reply: Option<&oneshot::Sender<Response<GrpcBody>>>,
) -> Response<GrpcBody> {
    let WorkerGate {
        upstream,
        coordinator,
        binding,
        remote,
        vault,
        ..
    } = gate;
    let remote = *remote;
    let poll = matches!(
        permit.track,
        PostTrack::PollWorkflow | PostTrack::PollActivity
    );
    let _pickup = if remote && (poll || permit.track == PostTrack::StartWorkflow) {
        if reply.is_some_and(oneshot::Sender::is_closed) {
            return grpc_error(1, "remote worker request canceled");
        }
        match coordinator.begin_remote_pickup() {
            Ok(guard) => Some(guard),
            Err(_) => {
                return grpc_error(
                    14,
                    "remote worker pickup is temporarily fenced for an update",
                );
            }
        }
    } else {
        None
    };

    let admission = if let Some((namespace, workflow_id)) = &permit.admit {
        let result = if permit.track == PostTrack::StartWorkflow {
            admit_worker_start(coordinator, &binding.owner, namespace, workflow_id)
        } else {
            admit_schedule(coordinator, namespace, workflow_id)
        };
        match result {
            Ok(admission) => Some(admission),
            Err((code, msg)) => return grpc_error(code, &msg),
        }
    } else {
        None
    };

    let is_workflow_completion = out_req
        .uri()
        .path()
        .ends_with("/RespondWorkflowTaskCompleted");
    let hide_worker_commands = (remote || vault.has_vault(&binding.owner))
        && out_req.uri().path().ends_with("/DescribeNamespace");
    let upstream_call = async {
        let (parts, body) = upstream.send(out_req).await?.into_parts();
        let buffered = Limited::new(body, MAX_GRPC_BODY_BYTES)
            .collect()
            .await
            .map_err(|_| "failed to collect upstream gRPC response".to_owned())?;
        Ok::<_, String>((parts, buffered.trailers().cloned(), buffered.to_bytes()))
    };
    let upstream_result = if remote && poll {
        // Temporal's normal poll deadline is one minute. Bound orphaned
        // RPCs even if the upstream connection has stopped making progress.
        tokio::time::timeout(std::time::Duration::from_secs(75), upstream_call)
            .await
            .unwrap_or_else(|_| Err("upstream remote poll timed out".into()))
    } else {
        upstream_call.await
    };
    let (mut resp_parts, trailers, mut resp_bytes) = match upstream_result {
        Ok(result) => result,
        Err(err) => {
            if let Some(admission) = admission {
                finish_admission(coordinator, admission, None);
            }
            return grpc_error(14, &err);
        }
    };
    let status = grpc_status_error(&resp_parts.headers, trailers.as_ref());
    let remote_success = resp_parts.status == StatusCode::OK && status.0 == 0;
    if (remote_success || status.0 == 5)
        && let Some(token) = permit.finish_local.as_deref()
    {
        coordinator.complete_lease(token);
    }

    if permit.track == PostTrack::StartWorkflow {
        let success = if remote {
            remote_success
        } else {
            !resp_Parts_has_grpc_error(&resp_parts.headers, trailers.as_ref())
        };
        let decoded = success
            .then(|| {
                unpack_unary_grpc(&resp_bytes)
                    .ok()
                    .and_then(|bytes| StartWorkflowExecutionResponse::decode(bytes).ok())
            })
            .flatten();
        let run_id = decoded
            .as_ref()
            .map(|resp| resp.run_id.clone())
            .filter(|id| !id.is_empty());
        if let Some(admission) = admission {
            finish_admission(coordinator, admission, run_id);
        }
        if remote {
            if !remote_success {
                return grpc_error(status.0.max(1), &status.1);
            }
            if decoded.is_none_or(|response| response.eager_workflow_task.is_some()) {
                return grpc_error(13, "unexpected remote workflow start response");
            }
        }
    } else if permit.admit.is_some() {
        if let Some(admission) = admission {
            finish_admission(coordinator, admission, None);
        }
    } else if poll {
        if remote && !remote_success {
            return grpc_error(status.0.max(1), &status.1);
        }
        let decoded = if resp_Parts_has_grpc_error(&resp_parts.headers, trailers.as_ref()) {
            None
        } else {
            unpack_unary_grpc(&resp_bytes)
                .ok()
                .and_then(|payload| decode_poll(permit.track, payload))
        };
        if let Some((token, execution, kind)) = decoded {
            if remote && token.is_empty() && execution.is_some() {
                return grpc_error(13, "remote poll returned an execution without a task token");
            }
            if !token.is_empty() {
                let (workflow_id, run_id) = execution
                    .map(|e| (e.workflow_id, e.run_id))
                    .unwrap_or_default();
                if remote {
                    if reply.is_some_and(oneshot::Sender::is_closed) {
                        return grpc_error(1, "remote worker request canceled");
                    }
                    if workflow_id.is_empty() || run_id.is_empty() {
                        return grpc_error(
                            13,
                            "remote poll returned a task without an execution identity",
                        );
                    }
                    if let Err(e) = coordinator.record_remote_lease(
                        &token,
                        kind,
                        &binding.task_queue,
                        TrackedKey {
                            namespace: binding.owner.clone(),
                            workflow_id,
                            run_id: Some(run_id),
                        },
                    ) {
                        return grpc_error(13, &e.detail());
                    }
                } else {
                    coordinator.record_lease(
                        token,
                        &binding.owner,
                        &binding.task_queue,
                        kind,
                        &workflow_id,
                        &run_id,
                    );
                }
            }
        } else if remote {
            return grpc_error(13, "invalid remote poll response");
        }
    }

    if remote && is_workflow_completion && remote_success {
        let decoded = unpack_unary_grpc(&resp_bytes)
            .ok()
            .and_then(|bytes| RespondWorkflowTaskCompletedResponse::decode(bytes).ok());
        if decoded
            .is_none_or(|resp| resp.workflow_task.is_some() || !resp.activity_tasks.is_empty())
        {
            // This was not a permitted pickup route; never forward
            // a task from an incompatible upstream implementation.
            return grpc_error(13, "unexpected remote workflow completion response");
        }
    }

    if let Some((kind, token)) = permit.finish_remote {
        // A token-scoped terminal worker response says the worker is done.
        // Temporal NotFound also settles it (e.g. if the task timed out),
        // but transport errors and other ambiguous failures do not.
        if (remote_success || (resp_parts.status == StatusCode::OK && status.0 == 5))
            && let Err(e) =
                coordinator.finish_remote_lease(&token, &binding.owner, &binding.task_queue, kind)
        {
            return grpc_error(13, &e.detail());
        }
    }
    if remote && is_workflow_completion && !remote_success {
        return grpc_error(status.0.max(1), &status.1);
    }

    if hide_worker_commands && remote_success {
        let decoded = unpack_unary_grpc(&resp_bytes)
            .ok()
            .and_then(|bytes| DescribeNamespaceResponse::decode(bytes).ok());
        let Some(mut response) = decoded else {
            return grpc_error(13, "invalid DescribeNamespace response");
        };
        if let Some(caps) = response
            .namespace_info
            .as_mut()
            .and_then(|info| info.capabilities.as_mut())
        {
            // These controls are delivered through a special Nexus poller.
            caps.worker_commands = false;
            resp_bytes = pack_unary_grpc(&response.encode_to_vec());
            resp_parts.headers.remove(http::header::CONTENT_LENGTH);
        }
    }

    let mut final_resp = Response::new(GrpcBody::with_parts(resp_bytes, trailers));
    *final_resp.status_mut() = resp_parts.status;
    *final_resp.headers_mut() = resp_parts.headers;
    final_resp
}

fn decode_poll(
    track: PostTrack,
    payload: &[u8],
) -> Option<(Vec<u8>, Option<WorkflowExecution>, RemoteTaskKind)> {
    match track {
        PostTrack::PollWorkflow => {
            let decoded = PollWorkflowTaskQueueResponse::decode(payload).ok()?;
            let kind = if decoded.query.is_some() {
                RemoteTaskKind::Query
            } else {
                RemoteTaskKind::Workflow
            };
            Some((decoded.task_token, decoded.workflow_execution, kind))
        }
        PostTrack::PollActivity => {
            let decoded = PollActivityTaskQueueResponse::decode(payload).ok()?;
            Some((
                decoded.task_token,
                decoded.workflow_execution,
                RemoteTaskKind::Activity,
            ))
        }
        PostTrack::StartWorkflow | PostTrack::None => None,
    }
}

#[allow(non_snake_case)]
fn resp_Parts_has_grpc_error(headers: &HeaderMap, trailers: Option<&HeaderMap>) -> bool {
    let status = trailers
        .and_then(|t| t.get("grpc-status"))
        .or_else(|| headers.get("grpc-status"));
    status.is_some_and(|val| val.as_bytes() != b"0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use temporalio_common::protos::temporal::api::{
        command::v1::{Command, StartChildWorkflowExecutionCommandAttributes},
        common::v1::WorkflowExecution,
        taskqueue::v1::TaskQueue,
        workflowservice::v1::{
            GetSystemInfoRequest, GetSystemInfoResponse, RespondWorkflowTaskCompletedResponse,
            StartWorkflowExecutionRequest, workflow_service_client::WorkflowServiceClient,
        },
    };
    use tonic::Code;

    fn pack_grpc_msg<M: Message>(msg: &M) -> Bytes {
        let encoded = msg.encode_to_vec();
        let mut out = Vec::with_capacity(5 + encoded.len());
        out.push(0);
        out.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        out.extend_from_slice(&encoded);
        Bytes::from(out)
    }

    #[tokio::test]
    async fn remote_fence_keeps_accepted_poll_counted_through_task_delivery() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap().to_string();
        let polled = Arc::new(Notify::new());
        let release_poll = Arc::new(Notify::new());
        let upstream_polled = Arc::clone(&polled);
        let upstream_release = Arc::clone(&release_poll);
        tokio::spawn(async move {
            let (stream, _) = upstream.accept().await.unwrap();
            let attempts = Arc::new(AtomicUsize::new(0));
            let service = service_fn(move |req: Request<Incoming>| {
                let polled = Arc::clone(&upstream_polled);
                let release = Arc::clone(&upstream_release);
                let attempts = Arc::clone(&attempts);
                async move {
                    let path = req.uri().path().to_owned();
                    let bytes = req.into_body().collect().await.unwrap().to_bytes();
                    let payload = unpack_unary_grpc(&bytes).unwrap();
                    let (data, status) = if path.ends_with("/PollWorkflowTaskQueue") {
                        let task_queue = PollWorkflowTaskQueueRequest::decode(payload)
                            .unwrap()
                            .task_queue
                            .unwrap();
                        assert_eq!(
                            task_queue.name,
                            remote_sticky_name(
                                &WorkerBinding {
                                    owner: "alice".into(),
                                    task_queue: "alice-greet".into(),
                                },
                                "same-sticky-name"
                            )
                        );
                        polled.notify_one();
                        release.notified().await;
                        (
                            pack_grpc_msg(&PollWorkflowTaskQueueResponse {
                                task_token: vec![1, 2, 3],
                                workflow_execution: Some(WorkflowExecution {
                                    workflow_id: "workflow".into(),
                                    run_id: "run".into(),
                                }),
                                ..Default::default()
                            }),
                            "0",
                        )
                    } else if path.ends_with("/RespondWorkflowTaskCompleted") {
                        let req = RespondWorkflowTaskCompletedRequest::decode(payload).unwrap();
                        assert!(!req.return_new_workflow_task);
                        assert!(matches!(req.commands[0].attributes.as_ref().unwrap(),
                            CommandAttrs::ScheduleActivityTaskCommandAttributes(a) if !a.request_eager_execution));
                        match attempts.fetch_add(1, Ordering::SeqCst) {
                            0 => (Bytes::new(), "14"),
                            1 => (
                                pack_grpc_msg(&RespondWorkflowTaskCompletedResponse {
                                    workflow_task: Some(PollWorkflowTaskQueueResponse {
                                        task_token: vec![4, 5, 6],
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }),
                                "0",
                            ),
                            _ => (
                                pack_grpc_msg(&RespondWorkflowTaskCompletedResponse::default()),
                                "0",
                            ),
                        }
                    } else {
                        panic!("a denied RPC reached upstream");
                    };
                    let mut trailers = HeaderMap::new();
                    trailers.insert("grpc-status", HeaderValue::from_static(status));
                    let mut response = Response::new(GrpcBody::with_parts(data, Some(trailers)));
                    response.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/grpc"),
                    );
                    Ok::<_, Infallible>(response)
                }
            });
            server_http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await
                .unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let coordinator = Arc::new(Coordinator::open(dir.path(), true).unwrap());
        let gate = Arc::new(WorkerGate {
            binding: Arc::new(WorkerBinding {
                owner: "alice".into(),
                task_queue: "alice-greet".into(),
            }),
            upstream: Arc::new(UpstreamPool::new(upstream_addr)),
            coordinator: Arc::clone(&coordinator),
            catalog: Arc::new(Catalog::from_yaml("", &Default::default()).unwrap()),
            vault: Arc::default(),
            remote: true,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                serve_worker_connection(stream, gate).await;
            }
        });
        let mut client = WorkflowServiceClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        let poll_request = PollWorkflowTaskQueueRequest {
            namespace: "alice".into(),
            task_queue: Some(TaskQueue {
                name: "same-sticky-name".into(),
                kind: TaskQueueKind::Sticky as i32,
                normal_name: "alice-greet".into(),
            }),
            ..Default::default()
        };
        let mut wrong_queue = poll_request.clone();
        wrong_queue.task_queue.as_mut().unwrap().normal_name = "alice-other".into();
        assert_eq!(
            client
                .poll_workflow_task_queue(wrong_queue)
                .await
                .unwrap_err()
                .code(),
            Code::PermissionDenied
        );
        let poll = tokio::spawn({
            let mut client = client.clone();
            let request = poll_request.clone();
            async move { client.poll_workflow_task_queue(request).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), polled.notified())
            .await
            .unwrap();
        coordinator.close_gate("update", "rev").await.unwrap();
        assert_eq!(coordinator.remote_drain("update", true).unwrap().pickups, 1);
        assert_eq!(
            client
                .poll_workflow_task_queue(poll_request.clone())
                .await
                .unwrap_err()
                .code(),
            Code::Unavailable
        );
        assert_eq!(
            client
                .poll_nexus_task_queue(PollNexusTaskQueueRequest {
                    namespace: "alice".into(),
                    task_queue: Some(TaskQueue {
                        name: "alice-greet".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .await
                .unwrap_err()
                .code(),
            Code::PermissionDenied
        );
        release_poll.notify_one();
        let task = poll.await.unwrap().unwrap().into_inner();
        let state = coordinator.remote_drain("update", false).unwrap();
        assert_eq!((state.pickups, state.leases), (0, 1));

        let response = RespondWorkflowTaskCompletedRequest {
            namespace: "alice".into(), task_token: task.task_token, return_new_workflow_task: true,
            commands: vec![Command {
                attributes: Some(CommandAttrs::ScheduleActivityTaskCommandAttributes(
                    temporalio_common::protos::temporal::api::command::v1::ScheduleActivityTaskCommandAttributes {
                        request_eager_execution: true, ..Default::default()
                    }
                )), ..Default::default()
            }], ..Default::default()
        };
        for expected in [Code::Unavailable, Code::Internal] {
            assert_eq!(
                client
                    .respond_workflow_task_completed(response.clone())
                    .await
                    .unwrap_err()
                    .code(),
                expected
            );
            assert_eq!(coordinator.remote_drain("update", false).unwrap().leases, 1);
        }
        client
            .respond_workflow_task_completed(response)
            .await
            .unwrap();
        assert_eq!(coordinator.remote_drain("update", false).unwrap().leases, 0);

        // Simulate cancellation of the HTTP/2 stream while the upstream RPC
        // is still pending. Losing the receiver is not a settled pickup.
        coordinator.reopen_gate("update").unwrap();
        let permit = authorize_worker_request(
            "PollWorkflowTaskQueue",
            &poll_request.encode_to_vec(),
            None,
            &gate,
        )
        .await
        .ok()
        .unwrap();
        let upstream_req = Request::post(format!(
            "http://{}{WORKFLOW_SERVICE_PREFIX}PollWorkflowTaskQueue",
            gate.upstream.addr,
        ))
        .version(http::Version::HTTP_2)
        .body(GrpcBody::with_parts(
            pack_unary_grpc(permit.rewritten.as_deref().unwrap()),
            None,
        ))
        .unwrap();
        let (reply, result) = oneshot::channel();
        let canceled_poll = tokio::spawn(async move {
            forward_worker_rpc(upstream_req, permit, &gate, Some(&reply)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), polled.notified())
            .await
            .unwrap();
        drop(result);
        coordinator.close_gate("next-update", "rev").await.unwrap();
        assert_eq!(
            coordinator
                .remote_drain("next-update", true)
                .unwrap()
                .pickups,
            1
        );
        release_poll.notify_one();
        assert_eq!(canceled_poll.await.unwrap().headers()["grpc-status"], "1");
        let state = coordinator.remote_drain("next-update", false).unwrap();
        assert_eq!((state.pickups, state.leases), (0, 0));
    }

    #[tokio::test]
    async fn test_worker_gate_enforces_namespace_queue_methods_and_leases() {
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap().to_string();

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = upstream_listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let svc = service_fn(|req: Request<Incoming>| async move {
                        let path = req.uri().path().to_owned();
                        let _ = req.into_body().collect().await;
                        let body_bytes = if path.ends_with("/GetSystemInfo") {
                            pack_grpc_msg(&GetSystemInfoResponse::default())
                        } else if path.ends_with("/PollWorkflowTaskQueue") {
                            pack_grpc_msg(&PollWorkflowTaskQueueResponse {
                                task_token: vec![9, 9, 9],
                                workflow_execution: Some(WorkflowExecution {
                                    workflow_id: "wf-leased".into(),
                                    run_id: "run-leased".into(),
                                }),
                                ..Default::default()
                            })
                        } else {
                            pack_grpc_msg(&RespondWorkflowTaskCompletedResponse::default())
                        };
                        let mut trailers = HeaderMap::new();
                        trailers.insert("grpc-status", HeaderValue::from_static("0"));
                        let mut resp =
                            Response::new(GrpcBody::with_parts(body_bytes, Some(trailers)));
                        resp.headers_mut().insert(
                            http::header::CONTENT_TYPE,
                            HeaderValue::from_static("application/grpc"),
                        );
                        Ok::<_, Infallible>(resp)
                    });
                    let _ = server_http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });

        let dir = std::env::temp_dir().join(format!("boreal-proxy-test-{}", std::process::id()));
        let coord = Arc::new(Coordinator::open(&dir, false).unwrap());
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();

        tokio::spawn(serve_worker_listener(
            proxy_listener,
            WorkerBinding {
                owner: "alice".into(),
                task_queue: "alice-greet".into(),
            },
            upstream_addr,
            Arc::clone(&coord),
            Arc::new(Catalog::from_yaml("", &std::collections::BTreeMap::new()).unwrap()),
            Arc::default(),
        ));

        let mut client = WorkflowServiceClient::connect(format!("http://{proxy_addr}"))
            .await
            .unwrap();

        let mut gzip_req = tonic::Request::new(GetSystemInfoRequest::default());
        gzip_req
            .metadata_mut()
            .insert("grpc-encoding", "gzip".parse().unwrap());
        let enc_err = client.get_system_info(gzip_req).await.unwrap_err();
        assert_eq!(enc_err.code(), Code::Unimplemented);
        assert!(enc_err.message().contains("grpc-encoding"));

        let mut id_req = tonic::Request::new(GetSystemInfoRequest::default());
        id_req
            .metadata_mut()
            .insert("grpc-encoding", "identity".parse().unwrap());
        client.get_system_info(id_req).await.unwrap();

        let poll = client
            .poll_workflow_task_queue(PollWorkflowTaskQueueRequest {
                namespace: "alice".into(),
                task_queue: Some(TaskQueue {
                    name: "alice-greet".into(),
                    kind: TaskQueueKind::Normal as i32,
                    normal_name: String::new(),
                }),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(poll.task_token, vec![9, 9, 9]);
        assert_eq!(coord.active_lease_count(), 1);

        // Cross-namespace poll must fail with PermissionDenied.
        let err = client
            .poll_workflow_task_queue(PollWorkflowTaskQueueRequest {
                namespace: "bob".into(),
                task_queue: Some(TaskQueue {
                    name: "bob-greet".into(),
                    kind: TaskQueueKind::Normal as i32,
                    normal_name: String::new(),
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);

        // Wrong queue in same namespace must fail with PermissionDenied.
        let err = client
            .poll_workflow_task_queue(PollWorkflowTaskQueueRequest {
                namespace: "alice".into(),
                task_queue: Some(TaskQueue {
                    name: "alice-count".into(),
                    kind: TaskQueueKind::Normal as i32,
                    normal_name: String::new(),
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);

        // Client RPCs like StartWorkflowExecution must fail on worker gate.
        let err = client
            .start_workflow_execution(StartWorkflowExecutionRequest {
                namespace: "alice".into(),
                workflow_id: "bypass".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);

        // Cross-namespace child workflow inside RespondWorkflowTaskCompleted must fail.
        #[allow(deprecated)]
        let cross_ns_cmd = Command {
            attributes: Some(CommandAttrs::StartChildWorkflowExecutionCommandAttributes(
                StartChildWorkflowExecutionCommandAttributes {
                    namespace: "bob".into(),
                    workflow_id: "evil-child".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let err = client
            .respond_workflow_task_completed(RespondWorkflowTaskCompletedRequest {
                namespace: "alice".into(),
                task_token: vec![9, 9, 9],
                commands: vec![cross_ns_cmd],
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(coord.active_lease_count(), 1);

        // Legitimate completion clears lease and records same-namespace child!
        #[allow(deprecated)]
        let same_ns_cmd = Command {
            attributes: Some(CommandAttrs::StartChildWorkflowExecutionCommandAttributes(
                StartChildWorkflowExecutionCommandAttributes {
                    namespace: "alice".into(),
                    workflow_id: "child-1".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        client
            .respond_workflow_task_completed(RespondWorkflowTaskCompletedRequest {
                namespace: "alice".into(),
                task_token: vec![9, 9, 9],
                commands: vec![same_ns_cmd],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(coord.active_lease_count(), 0);
        assert!(
            coord
                .tracked_runs_snapshot()
                .iter()
                .any(|k| k.workflow_id == "child-1")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn cross_user_catalog() -> Catalog {
        let workers = "\
rig-worker:
  workflow: RigWorkflow
  description: Shared rig
  args:
  - name: manifest
    type: string
private-worker:
  workflow: PrivateWorkflow
  description: Private
  args:
  - name: manifest
    type: string
";
        let mut users = std::collections::BTreeMap::new();
        users.insert(
            "alice".into(),
            "\
user: alice
permissions:
  read: false
  write: false
workers:
  prep:
    package: rig-worker
    permissions:
      read: false
      write: false
"
            .into(),
        );
        users.insert(
            "shared".into(),
            "\
user: shared
permissions:
  read: false
  write: false
workers:
  rig:
    package: rig-worker
    permissions:
      read: [alice]
      write: [alice]
  private:
    package: private-worker
    permissions:
      read: false
      write: false
"
            .into(),
        );
        Catalog::from_yaml(workers, &users).unwrap()
    }

    #[tokio::test]
    async fn test_vault_authorizes_the_operation_and_delivered_workflow_task() {
        use temporalio_common::protos::temporal::api::{
            command::v1::ScheduleActivityTaskCommandAttributes, common::v1::ActivityType,
        };

        let dir = std::env::temp_dir().join(format!("boreal-vault-test-{}", std::process::id()));
        let coordinator = Arc::new(Coordinator::open(&dir, true).unwrap());
        let catalog = Arc::new(cross_user_catalog());
        let vault = Arc::new(
            crate::vault::Policy::new(
                serde_json::from_str(r#"{"alice":{"prep":["verify-token"]}}"#).unwrap(),
                &catalog,
            )
            .unwrap(),
        );
        let gate = |owner: &str, task_queue: &str, remote| WorkerGate {
            binding: Arc::new(WorkerBinding {
                owner: owner.to_owned(),
                task_queue: task_queue.to_owned(),
            }),
            upstream: Arc::new(UpstreamPool::new("unused".into())),
            coordinator: Arc::clone(&coordinator),
            catalog: Arc::clone(&catalog),
            vault: Arc::clone(&vault),
            remote,
        };
        let req = |namespace: &str, operation: &str, token: u8, queue_kind: TaskQueueKind| {
            RespondWorkflowTaskCompletedRequest {
                namespace: namespace.into(),
                task_token: vec![token],
                return_new_workflow_task: true,
                commands: vec![Command {
                    attributes: Some(CommandAttrs::ScheduleActivityTaskCommandAttributes(
                        ScheduleActivityTaskCommandAttributes {
                            task_queue: Some(TaskQueue {
                                name: "alice-vault".into(),
                                kind: queue_kind.into(),
                                ..Default::default()
                            }),
                            activity_type: Some(ActivityType {
                                name: operation.into(),
                            }),
                            request_eager_execution: true,
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec()
        };
        coordinator.record_lease(
            vec![1],
            "alice",
            "alice-prep",
            RemoteTaskKind::Workflow,
            "job",
            "1",
        );
        coordinator.record_lease(
            vec![2],
            "alice",
            "alice-prep",
            RemoteTaskKind::Activity,
            "job",
            "1",
        );
        coordinator
            .record_remote_lease(
                &[3],
                RemoteTaskKind::Workflow,
                "alice-prep",
                TrackedKey {
                    namespace: "alice".into(),
                    workflow_id: "job".into(),
                    run_id: Some("1".into()),
                },
            )
            .unwrap();
        for (remote, token) in [(false, 1), (true, 3)] {
            let gate = gate("alice", "alice-prep", remote);
            let payload = req("alice", "verify-token", token, TaskQueueKind::Normal);
            let permit =
                authorize_worker_request("RespondWorkflowTaskCompleted", &payload, None, &gate)
                    .await
                    .unwrap();
            let forwarded =
                RespondWorkflowTaskCompletedRequest::decode(permit.rewritten.as_deref().unwrap())
                    .unwrap();
            assert!(!forwarded.return_new_workflow_task);
            assert!(matches!(forwarded.commands[0].attributes.as_ref(),
                Some(CommandAttrs::ScheduleActivityTaskCommandAttributes(attr)) if !attr.request_eager_execution));
        }
        for (owner, queue, operation, token, queue_kind, remote) in [
            (
                "alice",
                "alice-other",
                "verify-token",
                1,
                TaskQueueKind::Normal,
                false,
            ),
            (
                "shared",
                "shared-rig",
                "verify-token",
                1,
                TaskQueueKind::Normal,
                false,
            ),
            (
                "alice",
                "alice-prep",
                "return-token",
                1,
                TaskQueueKind::Normal,
                false,
            ),
            (
                "alice",
                "alice-prep",
                "verify-token",
                2,
                TaskQueueKind::Normal,
                false,
            ),
            (
                "alice",
                "alice-prep",
                "verify-token",
                1,
                TaskQueueKind::Sticky,
                false,
            ),
            (
                "alice",
                "alice-prep",
                "verify-token",
                1,
                TaskQueueKind::Normal,
                true,
            ),
        ] {
            let payload = req(owner, operation, token, queue_kind);
            assert!(matches!(
                authorize_worker_request(
                    "RespondWorkflowTaskCompleted",
                    &payload,
                    None,
                    &gate(owner, queue, remote)
                )
                .await,
                Err((7, _))
            ));
        }

        let unconfigured = WorkerGate {
            vault: Arc::default(),
            ..gate("alice", "alice-prep", false)
        };
        let payload = req("alice", "verify-token", 1, TaskQueueKind::Normal);
        assert!(matches!(
            authorize_worker_request(
                "RespondWorkflowTaskCompleted",
                &payload,
                None,
                &unconfigured
            )
            .await,
            Err((7, _))
        ));

        let poll = PollActivityTaskQueueRequest {
            namespace: "alice".into(),
            task_queue: Some(TaskQueue {
                name: "alice-vault".into(),
                ..Default::default()
            }),
            ..Default::default()
        }
        .encode_to_vec();
        assert!(
            authorize_worker_request(
                "PollActivityTaskQueue",
                &poll,
                None,
                &gate("alice", "alice-vault", false)
            )
            .await
            .is_ok()
        );
        for (method, queue, remote) in [
            ("PollActivityTaskQueue", "alice-prep", false),
            ("PollActivityTaskQueue", "alice-vault", true),
            ("PollWorkflowTaskQueue", "alice-vault", false),
        ] {
            assert!(matches!(
                authorize_worker_request(method, &poll, None, &gate("alice", queue, remote)).await,
                Err((7, _))
            ));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn described(task_queue: &str, workflow_type: &str, workflow_id: &str, run_id: &str) -> Bytes {
        use temporalio_common::protos::temporal::api::{
            common::v1::WorkflowType, workflow::v1::WorkflowExecutionInfo,
        };
        pack_grpc_msg(&DescribeWorkflowExecutionResponse {
            workflow_execution_info: Some(WorkflowExecutionInfo {
                execution: Some(WorkflowExecution {
                    workflow_id: workflow_id.into(),
                    run_id: run_id.into(),
                }),
                r#type: Some(WorkflowType {
                    name: workflow_type.into(),
                }),
                task_queue: task_queue.into(),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn test_worker_gate_authorizes_cross_namespace_start_history_and_cancel() {
        use temporalio_common::protos::temporal::api::{
            common::v1::WorkflowType,
            workflowservice::v1::{
                GetWorkflowExecutionHistoryResponse, RequestCancelWorkflowExecutionResponse,
                StartWorkflowExecutionResponse,
            },
        };

        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = upstream_listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let svc = service_fn(|req: Request<Incoming>| async move {
                        let path = req.uri().path().to_owned();
                        let collected = req.into_body().collect().await.unwrap().to_bytes();
                        let payload = unpack_unary_grpc(&collected).unwrap();
                        let (body_bytes, status) = if path.ends_with("/DescribeWorkflowExecution") {
                            let req = DescribeWorkflowExecutionRequest::decode(payload).unwrap();
                            let id = req.execution.unwrap().workflow_id;
                            if id == "rig-1" {
                                (
                                    described("shared-rig", "RigWorkflow", "rig-1", "run-1"),
                                    "0",
                                )
                            } else if id == "private-1" {
                                (
                                    described(
                                        "shared-private",
                                        "PrivateWorkflow",
                                        "private-1",
                                        "run-p",
                                    ),
                                    "0",
                                )
                            } else {
                                (Bytes::new(), "5")
                            }
                        } else if path.ends_with("/StartWorkflowExecution") {
                            (
                                pack_grpc_msg(&StartWorkflowExecutionResponse {
                                    run_id: "run-1".into(),
                                    ..Default::default()
                                }),
                                "0",
                            )
                        } else if path.ends_with("/GetWorkflowExecutionHistory") {
                            (
                                pack_grpc_msg(&GetWorkflowExecutionHistoryResponse::default()),
                                "0",
                            )
                        } else if path.ends_with("/RequestCancelWorkflowExecution") {
                            (
                                pack_grpc_msg(&RequestCancelWorkflowExecutionResponse::default()),
                                "0",
                            )
                        } else {
                            (Bytes::new(), "12")
                        };
                        let mut trailers = HeaderMap::new();
                        trailers.insert("grpc-status", HeaderValue::from_static(status));
                        let mut resp =
                            Response::new(GrpcBody::with_parts(body_bytes, Some(trailers)));
                        resp.headers_mut().insert(
                            http::header::CONTENT_TYPE,
                            HeaderValue::from_static("application/grpc"),
                        );
                        Ok::<_, Infallible>(resp)
                    });
                    let _ = server_http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "boreal-proxy-cross-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let coord = Arc::new(Coordinator::open(&dir, false).unwrap());
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(serve_worker_listener(
            proxy_listener,
            WorkerBinding {
                owner: "alice".into(),
                task_queue: "alice-prep".into(),
            },
            upstream_addr,
            Arc::clone(&coord),
            Arc::new(cross_user_catalog()),
            Arc::default(),
        ));

        let mut client = WorkflowServiceClient::connect(format!("http://{proxy_addr}"))
            .await
            .unwrap();
        let rig_queue = TaskQueue {
            name: "shared-rig".into(),
            kind: TaskQueueKind::Normal as i32,
            normal_name: String::new(),
        };

        let denied = client
            .start_workflow_execution(StartWorkflowExecutionRequest {
                namespace: "shared".into(),
                workflow_id: "private-start".into(),
                workflow_type: Some(WorkflowType {
                    name: "PrivateWorkflow".into(),
                }),
                task_queue: Some(TaskQueue {
                    name: "shared-private".into(),
                    kind: TaskQueueKind::Normal as i32,
                    normal_name: String::new(),
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied);

        client
            .start_workflow_execution(StartWorkflowExecutionRequest {
                namespace: "shared".into(),
                workflow_id: "rig-1".into(),
                workflow_type: Some(WorkflowType {
                    name: "RigWorkflow".into(),
                }),
                task_queue: Some(rig_queue.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(coord.tracked_runs_snapshot().iter().any(|key| {
            key.namespace == "shared"
                && key.workflow_id == "rig-1"
                && key.run_id.as_deref() == Some("run-1")
        }));

        client
            .get_workflow_execution_history(GetWorkflowExecutionHistoryRequest {
                namespace: "shared".into(),
                execution: Some(WorkflowExecution {
                    workflow_id: "rig-1".into(),
                    run_id: "run-1".into(),
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let denied = client
            .get_workflow_execution_history(GetWorkflowExecutionHistoryRequest {
                namespace: "shared".into(),
                execution: Some(WorkflowExecution {
                    workflow_id: "private-1".into(),
                    run_id: "run-p".into(),
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied);

        let denied = client
            .request_cancel_workflow_execution(RequestCancelWorkflowExecutionRequest {
                namespace: "shared".into(),
                workflow_execution: Some(WorkflowExecution {
                    workflow_id: "rig-1".into(),
                    run_id: "other-run".into(),
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied);
        client
            .request_cancel_workflow_execution(RequestCancelWorkflowExecutionRequest {
                namespace: "shared".into(),
                workflow_execution: Some(WorkflowExecution {
                    workflow_id: "rig-1".into(),
                    run_id: "run-1".into(),
                }),
                ..Default::default()
            })
            .await
            .unwrap();

        coord.close_gate("upd-1", "rev").await.unwrap();
        let denied = client
            .start_workflow_execution(StartWorkflowExecutionRequest {
                namespace: "shared".into(),
                workflow_id: "rig-closed".into(),
                workflow_type: Some(WorkflowType {
                    name: "RigWorkflow".into(),
                }),
                task_queue: Some(rig_queue.clone()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::FailedPrecondition);

        coord.record_lease(
            vec![7, 7],
            "alice",
            "alice-prep",
            RemoteTaskKind::Workflow,
            "",
            "",
        );
        client
            .start_workflow_execution(StartWorkflowExecutionRequest {
                namespace: "shared".into(),
                workflow_id: "rig-drain".into(),
                workflow_type: Some(WorkflowType {
                    name: "RigWorkflow".into(),
                }),
                task_queue: Some(rig_queue),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            coord
                .tracked_runs_snapshot()
                .iter()
                .any(|key| key.workflow_id == "rig-drain")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn greet_schedule(paused: bool) -> Schedule {
        use temporalio_common::protos::temporal::api::{
            common::v1::WorkflowType,
            schedule::v1::{ScheduleAction, ScheduleState},
        };
        Schedule {
            action: Some(ScheduleAction {
                action: Some(ScheduleActionKind::StartWorkflow(
                    NewWorkflowExecutionInfo {
                        workflow_id: "hourly-hello".into(),
                        workflow_type: Some(WorkflowType {
                            name: "DemoWorkflow".into(),
                        }),
                        task_queue: Some(TaskQueue {
                            name: "alice-greet".into(),
                            kind: TaskQueueKind::Normal as i32,
                            normal_name: String::new(),
                        }),
                        ..Default::default()
                    },
                )),
            }),
            state: Some(ScheduleState {
                paused,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_schedule_catchup_and_backfill_bounds() {
        let mut schedule = greet_schedule(false);
        clamp_catchup(&mut schedule);
        let window = schedule.policies.unwrap().catchup_window.unwrap();
        assert_eq!(window.seconds, MAX_CATCHUP_SECS);
        assert_eq!(window.nanos, 0);

        let mut wide = greet_schedule(false);
        wide.policies
            .get_or_insert_with(Default::default)
            .catchup_window
            .get_or_insert_with(Default::default)
            .seconds = 365 * 24 * 60 * 60;
        clamp_catchup(&mut wide);
        assert_eq!(
            wide.policies.unwrap().catchup_window.unwrap().seconds,
            MAX_CATCHUP_SECS
        );

        let mut backfill = BackfillRequest::default();
        backfill
            .start_time
            .get_or_insert_with(Default::default)
            .seconds = 1_000;
        backfill
            .end_time
            .get_or_insert_with(Default::default)
            .seconds = 1_000 + MAX_BACKFILL_SECS;
        assert!(backfill_allowed(&backfill));
        backfill.end_time.as_mut().unwrap().seconds += 1;
        assert!(!backfill_allowed(&backfill));
        assert!(!backfill_allowed(&BackfillRequest::default()));
    }

    #[tokio::test]
    async fn test_worker_gate_authorizes_schedule_rpcs_and_clamps_catchup() {
        use std::sync::Mutex as StdMutex;
        use temporalio_common::protos::temporal::api::{
            schedule::v1::SchedulePatch,
            workflowservice::v1::{
                CreateScheduleRequest, CreateScheduleResponse, DeleteScheduleRequest,
                DeleteScheduleResponse, DescribeScheduleResponse, ListSchedulesRequest,
                PatchScheduleRequest,
            },
        };

        let recorded = Arc::new(StdMutex::new(Vec::new()));
        let recorded_upstream = Arc::clone(&recorded);
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = upstream_listener.accept().await else {
                    break;
                };
                let recorded_upstream = Arc::clone(&recorded_upstream);
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let recorded_upstream = Arc::clone(&recorded_upstream);
                        async move {
                            let path = req.uri().path().to_owned();
                            let collected = req.into_body().collect().await.unwrap().to_bytes();
                            let payload = unpack_unary_grpc(&collected).unwrap_or(&[]);
                            let body_bytes = if path.ends_with("/CreateSchedule") {
                                let decoded = CreateScheduleRequest::decode(payload).unwrap();
                                let secs = decoded
                                    .schedule
                                    .and_then(|schedule| schedule.policies)
                                    .and_then(|policies| policies.catchup_window)
                                    .map(|window| window.seconds);
                                recorded_upstream.lock().unwrap().push(secs);
                                pack_grpc_msg(&CreateScheduleResponse::default())
                            } else if path.ends_with("/DescribeSchedule") {
                                pack_grpc_msg(&DescribeScheduleResponse {
                                    schedule: Some(greet_schedule(true)),
                                    ..Default::default()
                                })
                            } else if path.ends_with("/DeleteSchedule") {
                                pack_grpc_msg(&DeleteScheduleResponse::default())
                            } else {
                                Bytes::new()
                            };
                            let mut trailers = HeaderMap::new();
                            trailers.insert("grpc-status", HeaderValue::from_static("0"));
                            let mut resp =
                                Response::new(GrpcBody::with_parts(body_bytes, Some(trailers)));
                            resp.headers_mut().insert(
                                http::header::CONTENT_TYPE,
                                HeaderValue::from_static("application/grpc"),
                            );
                            Ok::<_, Infallible>(resp)
                        }
                    });
                    let _ = server_http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });

        let mut users = std::collections::BTreeMap::new();
        users.insert(
            "alice".into(),
            "user: alice\npermissions:\n  read: false\n  write: false\nworkers:\n  greet:\n    package: lab-worker\n    permissions:\n      read: false\n      write: false\n"
                .into(),
        );
        let catalog = Catalog::from_yaml(
            "lab-worker:\n  workflow: DemoWorkflow\n  args: []\n",
            &users,
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "boreal-proxy-sched-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let coord = Arc::new(Coordinator::open(&dir, false).unwrap());
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(serve_worker_listener(
            proxy_listener,
            WorkerBinding {
                owner: "alice".into(),
                task_queue: "alice-greet".into(),
            },
            upstream_addr,
            Arc::clone(&coord),
            Arc::new(catalog),
            Arc::default(),
        ));
        let mut client = WorkflowServiceClient::connect(format!("http://{proxy_addr}"))
            .await
            .unwrap();

        let mut created = greet_schedule(false);
        created
            .policies
            .get_or_insert_with(Default::default)
            .catchup_window
            .get_or_insert_with(Default::default)
            .seconds = 365 * 24 * 60 * 60;
        client
            .create_schedule(CreateScheduleRequest {
                namespace: "alice".into(),
                schedule_id: "hourly-hello".into(),
                schedule: Some(created),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            recorded.lock().unwrap().as_slice(),
            &[Some(MAX_CATCHUP_SECS)]
        );

        let mut wrong = greet_schedule(false);
        if let Some(action) = wrong.action.as_mut()
            && let Some(ScheduleActionKind::StartWorkflow(info)) = action.action.as_mut()
        {
            info.workflow_type = Some(
                temporalio_common::protos::temporal::api::common::v1::WorkflowType {
                    name: "OtherWorkflow".into(),
                },
            );
        }
        let denied = client
            .create_schedule(CreateScheduleRequest {
                namespace: "alice".into(),
                schedule_id: "hourly-hello".into(),
                schedule: Some(wrong),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied);

        let denied = client
            .list_schedules(ListSchedulesRequest {
                namespace: "bob".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied);

        coord.close_gate("upd-sched", "rev").await.unwrap();
        let denied = client
            .create_schedule(CreateScheduleRequest {
                namespace: "alice".into(),
                schedule_id: "hourly-hello".into(),
                schedule: Some(greet_schedule(false)),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::FailedPrecondition);

        client
            .create_schedule(CreateScheduleRequest {
                namespace: "alice".into(),
                schedule_id: "paused-hello".into(),
                schedule: Some(greet_schedule(true)),
                ..Default::default()
            })
            .await
            .unwrap();
        client
            .delete_schedule(DeleteScheduleRequest {
                namespace: "alice".into(),
                schedule_id: "hourly-hello".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let denied = client
            .patch_schedule(PatchScheduleRequest {
                namespace: "alice".into(),
                schedule_id: "hourly-hello".into(),
                patch: Some(SchedulePatch {
                    unpause: "resume".into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(denied.code(), Code::FailedPrecondition);
        assert_eq!(recorded.lock().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
