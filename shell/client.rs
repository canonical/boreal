use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use boreal_common::catalog::Catalog;
use boreal_common::wire::{
    self, PermissionReport, Request, Response, ResultOutcome, WorkerEntry, WorkflowDetail,
    WorkflowSummary,
};

pub enum Mode {
    Live { socket: PathBuf },
    Mock { catalog: Catalog, caller: String },
}

impl Mode {
    pub fn is_mock(&self) -> bool {
        matches!(self, Self::Mock { .. })
    }

    pub fn send(&self, req: &Request) -> Result<Response, String> {
        match self {
            Self::Live { socket } => send_ipc(socket, req),
            Self::Mock { catalog, caller } => Ok(mock_dispatch(catalog, caller, req)),
        }
    }
}

fn send_ipc(socket: &Path, req: &Request) -> Result<Response, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("connect broker {}: {e}", socket.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    wire::write_frame(&mut stream, &req.encode()).map_err(|e| e.detail())?;
    let raw = wire::read_frame(&mut stream).map_err(|e| e.detail())?;
    Response::decode(&raw).map_err(|e| e.detail())
}

fn mock_dispatch(catalog: &Catalog, caller: &str, req: &Request) -> Response {
    match req {
        Request::Workers => match catalog.list_accessible_workers(caller) {
            Ok(list) => Response::Workers(list),
            Err(e) => Response::Error {
                code: wire::ErrorCode::PermissionDenied,
                message: e.detail(),
            },
        },
        Request::Permissions { user } => {
            match catalog.inspect_permissions(caller, user.as_deref()) {
                Ok(r) => Response::Permissions(r),
                Err(e) => Response::Error {
                    code: wire::ErrorCode::PermissionDenied,
                    message: e.detail(),
                },
            }
        }
        Request::Start {
            owner,
            worker,
            id,
            args: _,
        } => {
            let wf_type = catalog
                .worker_and_package(owner, worker)
                .map_or_else(|_| "MockWorkflow".into(), |(_, p)| p.workflow_type().into());
            Response::Started {
                owner: owner.clone(),
                worker: worker.clone(),
                workflow_type: wf_type,
                workflow_id: id.clone(),
                run_id: format!("mock-{id}"),
            }
        }
        Request::List { owner } => Response::List(vec![WorkflowSummary {
            owner: owner.clone(),
            worker: "greet".into(),
            workflow_type: "DemoWorkflow".into(),
            workflow_id: "sample-1".into(),
            run_id: "mock-sample-1".into(),
            status: "running".into(),
        }]),
        Request::Status { owner, id } => Response::Status(WorkflowDetail {
            owner: owner.clone(),
            worker: "greet".into(),
            workflow_type: "DemoWorkflow".into(),
            task_queue: format!("{owner}-greet"),
            workflow_id: id.clone(),
            run_id: format!("mock-{id}"),
            status: "running".into(),
            start_time: "-".into(),
            close_time: "-".into(),
        }),
        Request::Result { id, .. } => Response::WorkflowResult(ResultOutcome::Completed {
            run_id: format!("mock-{id}"),
            output: "[mock result]".into(),
        }),
        Request::PrepareCancel { owner, id } => Response::CancelPrepared(WorkflowDetail {
            owner: owner.clone(),
            worker: "greet".into(),
            workflow_type: "DemoWorkflow".into(),
            task_queue: format!("{owner}-greet"),
            workflow_id: id.clone(),
            run_id: format!("mock-{id}"),
            status: "running".into(),
            start_time: "-".into(),
            close_time: "-".into(),
        }),
        Request::Cancel { owner, id, run_id } => Response::Cancelled {
            owner: owner.clone(),
            worker: "greet".into(),
            workflow_type: "DemoWorkflow".into(),
            workflow_id: id.clone(),
            run_id: run_id.clone(),
        },
    }
}

pub fn print_response(resp: Response, mock: bool) {
    let prefix = if mock { "[mock] " } else { "" };
    match resp {
        Response::Workers(items) => print_workers(&items, prefix),
        Response::Started {
            owner,
            worker,
            workflow_type,
            workflow_id,
            run_id,
        } => {
            println!(
                "{prefix}started {workflow_type} '{workflow_id}' on {owner}/{worker} (run ID: {run_id})"
            );
        }
        Response::List(items) => print_workflows(&items, prefix),
        Response::Status(d) => {
            println!(
                "{prefix}workflow ID: {}\nrun ID:      {}\nowner:       {}\nworker:      {} ({})\nstatus:      {}\nstarted:     {}\nclosed:      {}",
                d.workflow_id,
                d.run_id,
                d.owner,
                d.worker,
                d.workflow_type,
                d.status,
                d.start_time,
                d.close_time
            );
        }
        Response::WorkflowResult(outcome) => match outcome {
            ResultOutcome::Completed { run_id, output } => {
                println!("{prefix}completed (run ID: {run_id}):\n{output}");
            }
            ResultOutcome::Running { run_id } => {
                println!("{prefix}workflow is still running (run ID: {run_id}); no result yet.");
            }
            ResultOutcome::Unsuccessful {
                run_id,
                status,
                detail,
            } => {
                eprintln!("{prefix}workflow {status} (run ID: {run_id}): {detail}");
            }
        },
        Response::CancelPrepared(_) => {}
        Response::Cancelled {
            owner,
            worker,
            workflow_type,
            workflow_id,
            run_id,
        } => {
            println!(
                "{prefix}cancellation requested for {workflow_type} '{workflow_id}' on {owner}/{worker} (run ID: {run_id})"
            );
        }
        Response::Permissions(report) => print_permissions(&report, prefix),
        Response::Error { message, .. } => {
            eprintln!("error: {message}");
        }
    }
}

fn print_workers(items: &[WorkerEntry], prefix: &str) {
    if items.is_empty() {
        println!("{prefix}no accessible workers.");
        return;
    }
    for item in items {
        let access = match (item.read, item.write) {
            (true, true) => "read,write",
            (true, false) => "read",
            (false, true) => "write",
            (false, false) => "none",
        };
        println!(
            "{prefix}{}/{} ({}) [{}] {}",
            item.owner, item.worker, item.workflow_type, access, item.signature
        );
        if !item.description.is_empty() {
            println!("  {}", item.description);
        }
    }
}

fn print_workflows(items: &[WorkflowSummary], prefix: &str) {
    if items.is_empty() {
        println!("{prefix}no workflows found.");
        return;
    }
    for w in items {
        println!(
            "{prefix}{}  status={}  worker={}/{} ({})  run={}",
            w.workflow_id, w.status, w.owner, w.worker, w.workflow_type, w.run_id
        );
    }
}

fn print_permissions(report: &PermissionReport, prefix: &str) {
    println!("{prefix}permissions for {}:", report.user);
    if report.is_self
        && let (Some(r), Some(w)) = (&report.default_read, &report.default_write)
    {
        println!("  default: read={r}, write={w}");
    }
    if report.workers.is_empty() {
        println!("  (no accessible workers)");
        return;
    }
    for line in &report.workers {
        println!(
            "  {} (package {}): read={}, write={}",
            line.worker, line.package, line.read, line.write
        );
    }
}
