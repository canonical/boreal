use std::{io, time::Duration};

use boreal_worker::{
    connect_namespace,
    temporalio_client::{
        WorkflowGetResultOptions, WorkflowIdReusePolicy, WorkflowStartOptions,
        errors::WorkflowGetResultError,
    },
    vault_activity_options,
};
use demo_core::{
    DISPATCH_WORKER, Dispatch, Request, VaultChecks, activity_failure, credential_check_options,
    single_attempt,
};
use serde::{Deserialize, Serialize};
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, WorkflowContext, WorkflowResult,
    activities::{ActivityContext, ActivityError},
};

#[derive(Deserialize, Serialize)]
pub struct DeskResult {
    dispatch_namespace: String,
    dispatch_workflow_id: String,
    receipt: String,
}

struct DeskActivities;

fn dispatch_namespace(caller: &str) -> Result<&'static str, ActivityError> {
    match caller {
        "alice" => Ok("shared"),
        "local" => Ok("local"),
        _ => Err(io::Error::other("desk is only deployed in alice or local").into()),
    }
}

#[activities]
impl DeskActivities {
    #[activity(name = "arrange-dispatch")]
    async fn arrange_dispatch(
        ctx: ActivityContext,
        request: Request,
    ) -> Result<DeskResult, ActivityError> {
        let namespace = dispatch_namespace(&ctx.info().namespace)?;
        let desk_run_id = ctx
            .info()
            .workflow_run_id
            .as_deref()
            .ok_or_else(|| io::Error::other("dispatch requires a desk workflow run"))?;
        let workflow_id = format!("dispatch-{desk_run_id}");

        // A separate Temporal execution, not a child workflow. In the VM the
        // broker authorizes the start and history RPCs as the alice desk worker.
        tokio::time::timeout(Duration::from_secs(30), async {
            let client = connect_namespace(namespace)
                .await
                .map_err(|_| io::Error::other("could not connect to Temporal for dispatch"))?;
            let options = WorkflowStartOptions::new(
                format!("{namespace}-{DISPATCH_WORKER}"),
                workflow_id.clone(),
            )
            .execution_timeout(Duration::from_secs(20))
            .id_reuse_policy(WorkflowIdReusePolicy::RejectDuplicate)
            .build();
            let handle = client
                .start_workflow(Dispatch, request, options)
                .await
                .map_err(|_| {
                    io::Error::other(format!("could not start {namespace}/{workflow_id}"))
                })?;
            let receipt = handle
                .get_result(
                    WorkflowGetResultOptions::builder()
                        .follow_runs(false)
                        .build(),
                )
                .await
                .map_err(|error| {
                    // Report context without copying failures from another execution.
                    let message = match error {
                        WorkflowGetResultError::Failed(_) => {
                            "dispatch failed; check the dispatch vault and dispatch_key"
                        }
                        _ => "could not read the dispatch receipt",
                    };
                    io::Error::other(format!("{message}; inspect {namespace}/{workflow_id}"))
                })?;

            Ok(DeskResult {
                dispatch_namespace: namespace.to_owned(),
                dispatch_workflow_id: workflow_id.clone(),
                receipt,
            })
        })
        .await
        .map_err(|_| io::Error::other(format!("timed out waiting for {namespace}/{workflow_id}")))?
    }
}

#[workflow]
#[derive(Default)]
pub struct DeskWorkflow;

#[workflow_methods]
impl DeskWorkflow {
    #[run]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
        request: Request,
    ) -> WorkflowResult<DeskResult> {
        request.validate()?;
        let vault_options = vault_activity_options(ctx.namespace(), credential_check_options());
        ctx.execute_activity(VaultChecks::check_desk_badge, (), vault_options)
            .await
            .map_err(activity_failure)?;

        // Do not retry a workflow start or repeat it after an uncertain result.
        let options = ActivityOptions::with_schedule_to_close_timeout(Duration::from_secs(35))
            .retry_policy(single_attempt())
            .build();
        ctx.execute_activity(DeskActivities::arrange_dispatch, request, options)
            .await
            .map_err(activity_failure)
    }
}

boreal_worker::declare_worker! {
    package = "demo-desk",
    workflows = {
        export request: DeskWorkflow => "Check the desk badge and send a zone request to dispatch",
    },
    configure = |opts| opts
        .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
        .register_activities(DeskActivities),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_alice_to_shared_and_local_to_itself() {
        assert_eq!(dispatch_namespace("alice").unwrap(), "shared");
        assert_eq!(dispatch_namespace("local").unwrap(), "local");
        assert!(dispatch_namespace("shared").is_err());
    }
}
