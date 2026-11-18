use lab_core::{
    CountWorkflow, DEFAULT_WAIT_SECONDS, DemoWorkflow, Workload, connect, task_queue,
    validate_input,
};
use temporalio_client::{UntypedWorkflow, WorkflowDescribeOptions, WorkflowStartOptions};

const USAGE: &str =
    "usage: lab-client start greet|count WORKFLOW_ID NAME [WAIT_SECONDS] | status WORKFLOW_ID";

enum Command {
    Start {
        workload: Workload,
        id: String,
        input: lab_core::DemoInput,
    },
    Status {
        id: String,
    },
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let args: Vec<_> = args.into_iter().collect();
    match args.as_slice() {
        [command, id] if command == "status" && !id.trim().is_empty() => {
            Ok(Command::Status { id: id.clone() })
        }
        [command, kind, id, name] if command == "start" && !id.trim().is_empty() => {
            Ok(Command::Start {
                workload: parse_workload(kind)?,
                id: id.clone(),
                input: validate_input(name.clone(), DEFAULT_WAIT_SECONDS)
                    .map_err(|e| e.to_string())?,
            })
        }
        [command, kind, id, name, seconds] if command == "start" && !id.trim().is_empty() => {
            let seconds = seconds
                .parse()
                .map_err(|_| "WAIT_SECONDS must be an integer".to_owned())?;
            Ok(Command::Start {
                workload: parse_workload(kind)?,
                id: id.clone(),
                input: validate_input(name.clone(), seconds).map_err(|e| e.to_string())?,
            })
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn parse_workload(kind: &str) -> Result<Workload, String> {
    match kind {
        "greet" => Ok(Workload::Greet),
        "count" => Ok(Workload::Count),
        _ => Err(USAGE.to_owned()),
    }
}

#[tokio::main]
async fn main() -> lab_core::LabResult<()> {
    let command = parse_args(std::env::args().skip(1))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let client = connect().await?;
    match command {
        Command::Start {
            workload,
            id,
            input,
        } => {
            let options =
                WorkflowStartOptions::new(task_queue(workload.worker_name())?, id).build();
            match workload {
                Workload::Greet => {
                    let handle = client
                        .start_workflow(DemoWorkflow::run, input, options)
                        .await?;
                    println!("started; run ID: {:?}", handle.run_id());
                }
                Workload::Count => {
                    let handle = client
                        .start_workflow(CountWorkflow::run, input, options)
                        .await?;
                    println!("started; run ID: {:?}", handle.run_id());
                }
            }
        }
        Command::Status { id } => {
            let handle = client.get_workflow_handle::<UntypedWorkflow>(id);
            let description = handle.describe(WorkflowDescribeOptions::default()).await?;
            println!(
                "workflow ID: {}\nrun ID: {}\nstatus: {:?}",
                description.id(),
                description.run_id(),
                description.status()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parses_start_and_status_and_rejects_bad_arguments() {
        assert!(
            matches!(parse_args(args(&["start", "greet", "id", "Ada"])), Ok(Command::Start { workload: Workload::Greet, input, .. }) if input.wait_seconds == DEFAULT_WAIT_SECONDS)
        );
        assert!(
            matches!(parse_args(args(&["start", "count", "id", "Ada", "3"])), Ok(Command::Start { workload: Workload::Count, input, .. }) if input.wait_seconds == 3)
        );
        assert!(matches!(
            parse_args(args(&["status", "id"])),
            Ok(Command::Status { .. })
        ));
        for bad in [
            vec!["start", "greet", "id", "name", "0"],
            vec!["start", "count", "id", "name", "oops"],
            vec!["start", "unknown", "id", "name"],
            vec!["status", ""],
            vec!["status", "id", "extra"],
        ] {
            assert!(parse_args(args(&bad)).is_err());
        }
    }
}
