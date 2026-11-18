use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use boreal_common::catalog::Catalog;
use boreal_common::wire::{
    self, PermissionReport, PromptSummary, Request, Response, ResultOutcome, StepState,
    WorkerEntry, WorkflowCounts, WorkflowDetail, WorkflowEvent, WorkflowStep, WorkflowSummary,
    WorkflowView,
};

type MockRunningMap = BTreeMap<(String, String), (String, String)>;

pub enum Mode {
    Live {
        socket: PathBuf,
    },
    Mock {
        catalog: Catalog,
        caller: String,
        running: Mutex<MockRunningMap>,
    },
}

impl Mode {
    pub fn mock(catalog: Catalog, caller: impl Into<String>) -> Self {
        let mut seeded = MockRunningMap::new();
        for user in catalog.users().keys() {
            seeded.insert(
                (user.clone(), String::from("sample-1")),
                (String::from("greet"), String::from("DemoWorkflow")),
            );
        }
        Self::Mock {
            catalog,
            caller: caller.into(),
            running: Mutex::new(seeded),
        }
    }

    pub fn is_mock(&self) -> bool {
        matches!(self, Self::Mock { .. })
    }

    pub fn send(&self, req: &Request) -> Result<Response, String> {
        match self {
            Self::Live { socket } => send_ipc(
                socket,
                req,
                Duration::from_secs(30),
                Duration::from_secs(10),
            ),
            Self::Mock {
                catalog,
                caller,
                running,
            } => Ok(mock_dispatch(catalog, caller, running, req)),
        }
    }

    pub fn prompt_summary(&self) -> Option<PromptSummary> {
        match self {
            Self::Live { socket } => match send_ipc(
                socket,
                &Request::Summary,
                Duration::from_millis(250),
                Duration::from_millis(150),
            ) {
                Ok(Response::Summary(summary)) => Some(summary),
                _ => None,
            },
            Self::Mock {
                catalog,
                caller,
                running,
            } => match mock_dispatch(catalog, caller, running, &Request::Summary) {
                Response::Summary(summary) => Some(summary),
                _ => None,
            },
        }
    }
}

fn send_ipc(
    socket: &Path,
    req: &Request,
    read_timeout: Duration,
    write_timeout: Duration,
) -> Result<Response, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("connect broker {}: {e}", socket.display()))?;
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(write_timeout))
        .map_err(|e| e.to_string())?;
    wire::write_frame(&mut stream, &req.encode()).map_err(|e| e.detail())?;
    let raw = wire::read_frame(&mut stream).map_err(|e| e.detail())?;
    Response::decode(&raw).map_err(|e| e.detail())
}

fn mock_dispatch(
    catalog: &Catalog,
    caller: &str,
    running: &Mutex<MockRunningMap>,
    req: &Request,
) -> Response {
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
        Request::Run {
            owner,
            worker,
            workflow,
            id,
            args: _,
        } => {
            let wf_type = match catalog
                .worker_and_package(owner, worker)
                .and_then(|(_, pkg)| pkg.select(workflow.as_deref()))
            {
                Ok(workflow) => workflow.workflow_type().to_owned(),
                Err(e) => {
                    return Response::Error {
                        code: wire::ErrorCode::InvalidArgument,
                        message: e.detail(),
                    };
                }
            };
            running.lock().unwrap().insert(
                (owner.clone(), id.clone()),
                (worker.clone(), wf_type.clone()),
            );
            Response::Started {
                owner: owner.clone(),
                worker: worker.clone(),
                workflow_type: wf_type,
                workflow_id: id.clone(),
                run_id: format!("mock-{id}"),
            }
        }
        Request::List { owner } => {
            let items = running
                .lock()
                .unwrap()
                .iter()
                .filter(|((entry_owner, _), _)| entry_owner == owner)
                .map(|((_, wf_id), (worker, wf_type))| WorkflowSummary {
                    owner: owner.clone(),
                    worker: worker.clone(),
                    workflow_type: wf_type.clone(),
                    workflow_id: wf_id.clone(),
                    run_id: format!("mock-{wf_id}"),
                    status: "running".into(),
                })
                .collect();
            Response::List(items)
        }
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
        Request::Cancel { owner, id, run_id } => {
            running.lock().unwrap().remove(&(owner.clone(), id.clone()));
            Response::Cancelled {
                owner: owner.clone(),
                worker: "greet".into(),
                workflow_type: "DemoWorkflow".into(),
                workflow_id: id.clone(),
                run_id: run_id.clone(),
            }
        }
        Request::Summary => {
            let count = running
                .lock()
                .unwrap()
                .keys()
                .filter(|(entry_owner, _)| entry_owner == caller)
                .count() as u32;
            Response::Summary(PromptSummary {
                workflows: Some(WorkflowCounts {
                    paused: 0,
                    running: count,
                }),
                draining: false,
            })
        }
        Request::View { owner, id } => {
            let entry = running
                .lock()
                .unwrap()
                .get(&(owner.clone(), id.clone()))
                .cloned();
            let (worker, wf_type, status, steps, outcome) = match entry {
                Some((w, t)) => (
                    w,
                    t,
                    "running".into(),
                    vec![
                        WorkflowStep {
                            kind: "timer".into(),
                            name: "#1 (5.00s)".into(),
                            state: StepState::Completed,
                            duration: "5.00s".into(),
                            detail: String::new(),
                        },
                        WorkflowStep {
                            kind: "activity".into(),
                            name: "greet".into(),
                            state: StepState::Running,
                            duration: "1.20s elapsed".into(),
                            detail: "attempt 1".into(),
                        },
                    ],
                    None,
                ),
                None => (
                    "greet".into(),
                    "DemoWorkflow".into(),
                    "completed".into(),
                    vec![WorkflowStep {
                        kind: "activity".into(),
                        name: "greet".into(),
                        state: StepState::Completed,
                        duration: "38ms".into(),
                        detail: String::new(),
                    }],
                    Some(ResultOutcome::Completed {
                        run_id: format!("mock-{id}"),
                        output: "[mock result]".into(),
                    }),
                ),
            };
            Response::View(WorkflowView {
                owner: owner.clone(),
                worker: worker.clone(),
                workflow_type: wf_type.clone(),
                workflow_id: id.clone(),
                run_id: format!("mock-{id}"),
                status,
                start_time: "2026-11-16 14:02:10 UTC".into(),
                close_time: "-".into(),
                elapsed: "6.20s".into(),
                input: r#"{"name":"Alice","wait_seconds":5}"#.into(),
                task_warning: String::new(),
                steps,
                events: vec![
                    WorkflowEvent {
                        id: 1,
                        timestamp: "14:02:10.000".into(),
                        kind: "WorkflowStarted".into(),
                        summary: format!("{wf_type} on {owner}-{worker}"),
                    },
                    WorkflowEvent {
                        id: 2,
                        timestamp: "14:02:15.000".into(),
                        kind: "ActivityStarted".into(),
                        summary: "greet (attempt 1)".into(),
                    },
                ],
                outcome,
            })
        }
    }
}

pub fn print_response(resp: Response, mock: bool) -> bool {
    let prefix = if mock { "[mock] " } else { "" };
    match resp {
        Response::Workers(items) => {
            print_workers(&items, prefix);
            true
        }
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
            true
        }
        Response::List(items) => {
            print_workflows(&items, prefix);
            true
        }
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
            true
        }
        Response::WorkflowResult(outcome) => match outcome {
            ResultOutcome::Completed { run_id, output } => {
                println!("{prefix}completed (run ID: {run_id}):\n{output}");
                true
            }
            ResultOutcome::Running { run_id } => {
                println!("{prefix}workflow is still running (run ID: {run_id}); no result yet.");
                true
            }
            ResultOutcome::Unsuccessful {
                run_id,
                status,
                detail,
            } => {
                eprintln!("{prefix}workflow {status} (run ID: {run_id}): {detail}");
                false
            }
        },
        Response::CancelPrepared(_) => true,
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
            true
        }
        Response::Permissions(report) => {
            print_permissions(&report, prefix);
            true
        }
        Response::Error { message, .. } => {
            eprintln!("error: {message}");
            false
        }
        Response::Summary(_) | Response::View(_) => true,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn test_mock_and_live_prompt_summary_tracking_and_fallback() {
        let mock = Mode::mock(Catalog::mock(), "alice");
        assert_eq!(
            mock.prompt_summary(),
            Some(PromptSummary {
                workflows: Some(WorkflowCounts {
                    paused: 0,
                    running: 1,
                }),
                draining: false,
            })
        );

        mock.send(&Request::Run {
            owner: "alice".into(),
            worker: "greet".into(),
            workflow: None,
            id: "job-2".into(),
            args: vec!["Ada".into()],
        })
        .unwrap();
        assert_eq!(
            mock.prompt_summary().unwrap().workflows,
            Some(WorkflowCounts {
                paused: 0,
                running: 2,
            })
        );

        mock.send(&Request::Cancel {
            owner: "alice".into(),
            id: "sample-1".into(),
            run_id: "mock-sample-1".into(),
        })
        .unwrap();
        mock.send(&Request::Cancel {
            owner: "alice".into(),
            id: "job-2".into(),
            run_id: "mock-job-2".into(),
        })
        .unwrap();
        assert_eq!(
            mock.prompt_summary().unwrap().workflows,
            Some(WorkflowCounts {
                paused: 0,
                running: 0,
            })
        );

        // Missing socket fails silently to None without blocking login.
        let offline = Mode::Live {
            socket: std::env::temp_dir().join("nonexistent-boreal-broker.sock"),
        };
        assert_eq!(offline.prompt_summary(), None);

        // Live Unix socket summary round-trips paused/running counts and draining state.
        let sock =
            std::env::temp_dir().join(format!("boreal-client-sum-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let raw = wire::read_frame(&mut conn).unwrap();
            assert_eq!(Request::decode(&raw).unwrap(), Request::Summary);
            let reply = Response::Summary(PromptSummary {
                workflows: Some(WorkflowCounts {
                    paused: 1,
                    running: 4,
                }),
                draining: true,
            });
            wire::write_frame(&mut conn, &reply.encode()).unwrap();
        });
        let live = Mode::Live {
            socket: sock.clone(),
        };
        assert_eq!(
            live.prompt_summary(),
            Some(PromptSummary {
                workflows: Some(WorkflowCounts {
                    paused: 1,
                    running: 4,
                }),
                draining: true,
            })
        );
        server.join().unwrap();
        let _ = std::fs::remove_file(&sock);
    }
}
