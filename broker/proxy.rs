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
    workflow::v1::WorkflowExecutionInfo,
    workflowservice::v1::{
        DescribeNamespaceRequest, DescribeWorkflowExecutionRequest,
        DescribeWorkflowExecutionResponse, GetWorkflowExecutionHistoryRequest,
        PollActivityTaskQueueRequest, PollActivityTaskQueueResponse, PollNexusTaskQueueRequest,
        PollWorkflowTaskQueueRequest, PollWorkflowTaskQueueResponse,
        RecordActivityTaskHeartbeatByIdRequest, RecordActivityTaskHeartbeatRequest,
        RecordWorkerHeartbeatRequest, RequestCancelWorkflowExecutionRequest,
        RespondActivityTaskCanceledByIdRequest, RespondActivityTaskCanceledRequest,
        RespondActivityTaskCompletedByIdRequest, RespondActivityTaskCompletedRequest,
        RespondActivityTaskFailedByIdRequest, RespondActivityTaskFailedRequest,
        RespondNexusTaskCompletedRequest, RespondNexusTaskFailedRequest,
        RespondQueryTaskCompletedRequest, RespondWorkflowTaskCompletedRequest,
        RespondWorkflowTaskFailedRequest, ShutdownWorkerRequest, StartWorkflowExecutionRequest,
        StartWorkflowExecutionResponse,
    },
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use boreal_common::catalog::{self, Catalog};

use crate::ledger::{self, Coordinator, TrackedKey};

const MAX_GRPC_BODY_BYTES: usize = 16 * 1024 * 1024;
const WORKFLOW_SERVICE_PREFIX: &str = "/temporal.api.workflowservice.v1.WorkflowService/";

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
) {
    let binding = Arc::new(binding);
    let upstream = Arc::new(UpstreamPool::new(upstream_addr));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let binding = Arc::clone(&binding);
        let upstream = Arc::clone(&upstream);
        let coordinator = Arc::clone(&coordinator);
        let catalog = Arc::clone(&catalog);
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let binding = Arc::clone(&binding);
                let upstream = Arc::clone(&upstream);
                let coordinator = Arc::clone(&coordinator);
                let catalog = Arc::clone(&catalog);
                async move {
                    Ok::<_, Infallible>(
                        handle_worker_rpc(req, &binding, &upstream, &coordinator, &catalog).await,
                    )
                }
            });
            let _ = server_http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
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
    binding: &WorkerBinding,
    coordinator: &Coordinator,
    catalog: &Catalog,
    upstream: &UpstreamPool,
) -> Result<PostTrack, (u8, String)> {
    let deny = |msg: &str| (7u8, msg.to_owned());
    let bad = |msg: &str| (3u8, msg.to_owned());

    let check_ns = |ns: &str| -> Result<(), (u8, String)> {
        header_matches(header_ns, ns)?;
        if ns == binding.owner {
            Ok(())
        } else {
            Err(deny("worker cross-namespace access denied"))
        }
    };

    match method {
        "GetSystemInfo" => Ok(PostTrack::None),
        "DescribeNamespace" => {
            let req = DescribeNamespaceRequest::decode(payload)
                .map_err(|_| bad("invalid DescribeNamespaceRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "PollWorkflowTaskQueue" => {
            let req = PollWorkflowTaskQueueRequest::decode(payload)
                .map_err(|_| bad("invalid PollWorkflowTaskQueueRequest"))?;
            check_ns(&req.namespace)?;
            let Some(tq) = req.task_queue.as_ref() else {
                return Err(bad("missing task_queue"));
            };
            if tq.kind() == TaskQueueKind::Sticky {
                if !tq.normal_name.is_empty() && tq.normal_name != binding.task_queue {
                    return Err(deny("sticky poll normal_name mismatch"));
                }
            } else if tq.name != binding.task_queue {
                return Err(deny("worker task_queue access denied"));
            }
            Ok(PostTrack::PollWorkflow)
        }
        "PollActivityTaskQueue" => {
            let req = PollActivityTaskQueueRequest::decode(payload)
                .map_err(|_| bad("invalid PollActivityTaskQueueRequest"))?;
            check_ns(&req.namespace)?;
            let Some(tq) = req.task_queue.as_ref() else {
                return Err(bad("missing task_queue"));
            };
            if tq.name != binding.task_queue {
                return Err(deny("worker activity task_queue access denied"));
            }
            Ok(PostTrack::PollActivity)
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
            Ok(PostTrack::None)
        }
        "RespondWorkflowTaskCompleted" => {
            let req = RespondWorkflowTaskCompletedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondWorkflowTaskCompletedRequest"))?;
            check_ns(&req.namespace)?;
            let mut child_keys = Vec::new();
            for cmd in &req.commands {
                match &cmd.attributes {
                    Some(CommandAttrs::StartChildWorkflowExecutionCommandAttributes(attr)) => {
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
                    _ => {}
                }
            }
            if !child_keys.is_empty() {
                let _ = coordinator.track_runs(child_keys);
            }
            coordinator.complete_lease(&req.task_token);
            Ok(PostTrack::None)
        }
        "RespondWorkflowTaskFailed" => {
            let req = RespondWorkflowTaskFailedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondWorkflowTaskFailedRequest"))?;
            check_ns(&req.namespace)?;
            coordinator.complete_lease(&req.task_token);
            Ok(PostTrack::None)
        }
        "RespondQueryTaskCompleted" => {
            let req = RespondQueryTaskCompletedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondQueryTaskCompletedRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "RecordActivityTaskHeartbeat" => {
            let req = RecordActivityTaskHeartbeatRequest::decode(payload)
                .map_err(|_| bad("invalid RecordActivityTaskHeartbeatRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "RecordActivityTaskHeartbeatById" => {
            let req = RecordActivityTaskHeartbeatByIdRequest::decode(payload)
                .map_err(|_| bad("invalid RecordActivityTaskHeartbeatByIdRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "RespondActivityTaskCompleted" => {
            let req = RespondActivityTaskCompletedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskCompletedRequest"))?;
            check_ns(&req.namespace)?;
            coordinator.complete_lease(&req.task_token);
            Ok(PostTrack::None)
        }
        "RespondActivityTaskCompletedById" => {
            let req = RespondActivityTaskCompletedByIdRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskCompletedByIdRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "RespondActivityTaskFailed" => {
            let req = RespondActivityTaskFailedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskFailedRequest"))?;
            check_ns(&req.namespace)?;
            coordinator.complete_lease(&req.task_token);
            Ok(PostTrack::None)
        }
        "RespondActivityTaskFailedById" => {
            let req = RespondActivityTaskFailedByIdRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskFailedByIdRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "RespondActivityTaskCanceled" => {
            let req = RespondActivityTaskCanceledRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskCanceledRequest"))?;
            check_ns(&req.namespace)?;
            coordinator.complete_lease(&req.task_token);
            Ok(PostTrack::None)
        }
        "RespondActivityTaskCanceledById" => {
            let req = RespondActivityTaskCanceledByIdRequest::decode(payload)
                .map_err(|_| bad("invalid RespondActivityTaskCanceledByIdRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "RespondNexusTaskCompleted" => {
            let req = RespondNexusTaskCompletedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondNexusTaskCompletedRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "RespondNexusTaskFailed" => {
            let req = RespondNexusTaskFailedRequest::decode(payload)
                .map_err(|_| bad("invalid RespondNexusTaskFailedRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        "GetWorkflowExecutionHistory" => {
            let req = GetWorkflowExecutionHistoryRequest::decode(payload)
                .map_err(|_| bad("invalid GetWorkflowExecutionHistoryRequest"))?;
            header_matches(header_ns, &req.namespace)?;
            if req.namespace == binding.owner {
                return Ok(PostTrack::None);
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
            Ok(PostTrack::None)
        }
        "StartWorkflowExecution" => {
            let req = StartWorkflowExecutionRequest::decode(payload)
                .map_err(|_| bad("invalid StartWorkflowExecutionRequest"))?;
            header_matches(header_ns, &req.namespace)?;
            authorize_start(catalog, &binding.owner, &req)?;
            Ok(PostTrack::StartWorkflow)
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
            Ok(PostTrack::None)
        }
        "ShutdownWorker" => {
            let req = ShutdownWorkerRequest::decode(payload)
                .map_err(|_| bad("invalid ShutdownWorkerRequest"))?;
            check_ns(&req.namespace)?;
            if !req.task_queue.is_empty() && req.task_queue != binding.task_queue {
                return Err(deny("worker shutdown task_queue mismatch"));
            }
            Ok(PostTrack::None)
        }
        "RecordWorkerHeartbeat" => {
            let req = RecordWorkerHeartbeatRequest::decode(payload)
                .map_err(|_| bad("invalid RecordWorkerHeartbeatRequest"))?;
            check_ns(&req.namespace)?;
            Ok(PostTrack::None)
        }
        _ => Err(deny("RPC method is not permitted on Boreal worker gate")),
    }
}

fn header_matches(header_ns: Option<&str>, namespace: &str) -> Result<(), (u8, String)> {
    if let Some(header) = header_ns
        && header != namespace
    {
        return Err((7, "temporal-namespace header mismatch".to_owned()));
    }
    Ok(())
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
    if workflow_type != pkg.workflow_type() || !catalog.can_write(caller, &req.namespace, worker) {
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

async fn handle_worker_rpc(
    req: Request<Incoming>,
    binding: &WorkerBinding,
    upstream: &UpstreamPool,
    coordinator: &Coordinator,
    catalog: &Catalog,
) -> Response<GrpcBody> {
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

    let post_track = match authorize_worker_request(
        method,
        payload,
        header_ns.as_deref(),
        binding,
        coordinator,
        catalog,
        upstream,
    )
    .await
    {
        Ok(mode) => mode,
        Err((code, msg)) => return grpc_error(code, &msg),
    };

    let mut admission = if post_track == PostTrack::StartWorkflow {
        let Ok(start) = StartWorkflowExecutionRequest::decode(payload) else {
            return grpc_error(3, "invalid StartWorkflowExecutionRequest");
        };
        match admit_worker_start(
            coordinator,
            &binding.owner,
            &start.namespace,
            &start.workflow_id,
        ) {
            Ok(admission) => Some(admission),
            Err((code, msg)) => return grpc_error(code, &msg),
        }
    } else {
        None
    };

    let Ok(uri) = Uri::builder()
        .scheme("http")
        .authority(upstream.addr.as_str())
        .path_and_query(path.as_str())
        .build()
    else {
        return grpc_error(13, "failed to construct upstream URI");
    };

    let mut out_req = Request::new(GrpcBody::with_parts(req_bytes, None));
    *out_req.method_mut() = http::Method::POST;
    *out_req.uri_mut() = uri;
    *out_req.version_mut() = http::Version::HTTP_2;
    for (name, val) in &parts.headers {
        if name == http::header::HOST || name == "grpc-accept-encoding" {
            continue;
        }
        out_req.headers_mut().append(name.clone(), val.clone());
    }

    let upstream_resp = match upstream.send(out_req).await {
        Ok(r) => r,
        Err(err) => {
            if let Some(admission) = admission {
                finish_admission(coordinator, admission, None);
            }
            return grpc_error(14, &err);
        }
    };

    let (resp_parts, resp_body) = upstream_resp.into_parts();
    let Ok(buffered) = Limited::new(resp_body, MAX_GRPC_BODY_BYTES).collect().await else {
        if let Some(admission) = admission {
            finish_admission(coordinator, admission, None);
        }
        return grpc_error(14, "failed to collect upstream gRPC response");
    };
    let trailers = buffered.trailers().cloned();
    let resp_bytes = buffered.to_bytes();

    if post_track == PostTrack::StartWorkflow {
        let run_id = if !resp_Parts_has_grpc_error(&resp_parts.headers, trailers.as_ref())
            && let Ok(resp_proto) = unpack_unary_grpc(&resp_bytes)
            && let Ok(decoded) = StartWorkflowExecutionResponse::decode(resp_proto)
            && !decoded.run_id.is_empty()
        {
            Some(decoded.run_id)
        } else {
            None
        };
        if let Some(admission) = admission.take() {
            finish_admission(coordinator, admission, run_id);
        }
    } else if post_track != PostTrack::None
        && !resp_Parts_has_grpc_error(&resp_parts.headers, trailers.as_ref())
        && let Ok(resp_proto) = unpack_unary_grpc(&resp_bytes)
    {
        match post_track {
            PostTrack::PollWorkflow => {
                if let Ok(decoded) = PollWorkflowTaskQueueResponse::decode(resp_proto)
                    && !decoded.task_token.is_empty()
                {
                    let (wf_id, run_id) = decoded
                        .workflow_execution
                        .map(|e| (e.workflow_id, e.run_id))
                        .unwrap_or_default();
                    coordinator.record_lease(decoded.task_token, &binding.owner, &wf_id, &run_id);
                }
            }
            PostTrack::PollActivity => {
                if let Ok(decoded) = PollActivityTaskQueueResponse::decode(resp_proto)
                    && !decoded.task_token.is_empty()
                {
                    let (wf_id, run_id) = decoded
                        .workflow_execution
                        .map(|e| (e.workflow_id, e.run_id))
                        .unwrap_or_default();
                    coordinator.record_lease(decoded.task_token, &binding.owner, &wf_id, &run_id);
                }
            }
            PostTrack::StartWorkflow | PostTrack::None => {}
        }
    }

    let mut final_resp = Response::new(GrpcBody::with_parts(resp_bytes, trailers));
    *final_resp.status_mut() = resp_parts.status;
    *final_resp.headers_mut() = resp_parts.headers;
    final_resp
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
        let coord = Arc::new(Coordinator::open(&dir).unwrap());
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
  workflowType: RigWorkflow
  description: Shared rig
  args:
  - name: manifest
    type: string
private-worker:
  workflowType: PrivateWorkflow
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
        let coord = Arc::new(Coordinator::open(&dir).unwrap());
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

        coord.record_lease(vec![7, 7], "alice", "", "");
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
}
