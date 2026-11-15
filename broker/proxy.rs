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
    enums::v1::TaskQueueKind,
    workflowservice::v1::{
        DescribeNamespaceRequest, GetWorkflowExecutionHistoryRequest, PollActivityTaskQueueRequest,
        PollActivityTaskQueueResponse, PollNexusTaskQueueRequest, PollWorkflowTaskQueueRequest,
        PollWorkflowTaskQueueResponse, RecordActivityTaskHeartbeatByIdRequest,
        RecordActivityTaskHeartbeatRequest, RecordWorkerHeartbeatRequest,
        RespondActivityTaskCanceledByIdRequest, RespondActivityTaskCanceledRequest,
        RespondActivityTaskCompletedByIdRequest, RespondActivityTaskCompletedRequest,
        RespondActivityTaskFailedByIdRequest, RespondActivityTaskFailedRequest,
        RespondNexusTaskCompletedRequest, RespondNexusTaskFailedRequest,
        RespondQueryTaskCompletedRequest, RespondWorkflowTaskCompletedRequest,
        RespondWorkflowTaskFailedRequest, ShutdownWorkerRequest,
    },
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::ledger::{Coordinator, TrackedKey};

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
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let binding = Arc::clone(&binding);
                let upstream = Arc::clone(&upstream);
                let coordinator = Arc::clone(&coordinator);
                async move {
                    Ok::<_, Infallible>(
                        handle_worker_rpc(req, &binding, &upstream, &coordinator).await,
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
    headers.insert(
        "grpc-accept-encoding",
        HeaderValue::from_static("identity"),
    );
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
}

#[allow(deprecated)]
fn authorize_worker_request(
    method: &str,
    payload: &[u8],
    binding: &WorkerBinding,
    coordinator: &Coordinator,
) -> Result<PostTrack, (u8, String)> {
    let deny = |msg: &str| (7u8, msg.to_owned());
    let bad = |msg: &str| (3u8, msg.to_owned());

    let check_ns = |ns: &str| -> Result<(), (u8, String)> {
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
            check_ns(&req.namespace)?;
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

async fn handle_worker_rpc(
    req: Request<Incoming>,
    binding: &WorkerBinding,
    upstream: &UpstreamPool,
    coordinator: &Coordinator,
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
    if let Some(ns_hdr) = req.headers().get("temporal-namespace") {
        let Ok(ns_str) = ns_hdr.to_str() else {
            return grpc_error(3, "invalid temporal-namespace header");
        };
        if ns_str != binding.owner {
            return grpc_error(7, "temporal-namespace header mismatch");
        }
    }

    let (parts, incoming) = req.into_parts();
    let Ok(collected) = Limited::new(incoming, MAX_GRPC_BODY_BYTES).collect().await else {
        return grpc_error(3, "failed to read gRPC request body");
    };
    let req_bytes = collected.to_bytes();
    let payload = match unpack_unary_grpc(&req_bytes) {
        Ok(slice) => slice,
        Err((code, msg)) => return grpc_error(code, msg),
    };

    let post_track = match authorize_worker_request(method, payload, binding, coordinator) {
        Ok(mode) => mode,
        Err((code, msg)) => return grpc_error(code, &msg),
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
        Err(err) => return grpc_error(14, &err),
    };

    let (resp_parts, resp_body) = upstream_resp.into_parts();
    let Ok(buffered) = Limited::new(resp_body, MAX_GRPC_BODY_BYTES).collect().await else {
        return grpc_error(14, "failed to collect upstream gRPC response");
    };
    let trailers = buffered.trailers().cloned();
    let resp_bytes = buffered.to_bytes();

    if post_track != PostTrack::None
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
            PostTrack::None => {}
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
}
