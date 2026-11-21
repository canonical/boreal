use boreal_sdk::worker;
use demo_core::{
    DISPATCH_WORKFLOW_TYPE, Request, VaultChecks, activity_failure, credential_check_options,
};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{WorkflowContext, WorkflowResult};

fn receipt(request: &Request) -> String {
    format!(
        "Dispatch confirmed for {} in the {} zone.",
        request.requester, request.zone
    )
}

#[workflow]
#[derive(Default)]
pub struct DispatchWorkflow;

#[workflow_methods]
impl DispatchWorkflow {
    #[run(name = DISPATCH_WORKFLOW_TYPE)]
    pub async fn run(ctx: &mut WorkflowContext<Self>, request: Request) -> WorkflowResult<String> {
        request.validate()?;
        ctx.execute_activity(
            VaultChecks::check_dispatch_key,
            (),
            credential_check_options(ctx.namespace()),
        )
        .await
        .map_err(activity_failure)?;
        Ok(receipt(&request))
    }
}

worker::main! {
    package = "demo-dispatch",
    workflows = {
        export receipt: DispatchWorkflow => "Check the dispatch key and issue a zone receipt",
    },
    configure = |opts| opts.graceful_shutdown_period(worker::DEFAULT_SHUTDOWN_GRACE),
}

#[cfg(test)]
mod tests {
    use super::*;
    use demo_core::{Dispatch, Zone};
    use temporalio_common::WorkflowDefinition;

    #[test]
    fn registered_workflow_matches_the_desk_contract() {
        fn workflow_name(
            workflow: impl WorkflowDefinition<Input = Request, Output = String>,
        ) -> String {
            workflow.name().to_owned()
        }
        assert_eq!(
            workflow_name(DispatchWorkflow::run),
            workflow_name(Dispatch)
        );
        assert_eq!(
            receipt(&Request {
                requester: "Alice".into(),
                zone: Zone::North,
            }),
            "Dispatch confirmed for Alice in the north zone."
        );
    }
}
