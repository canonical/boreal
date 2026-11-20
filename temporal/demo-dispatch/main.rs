use boreal_worker::vault_activity_options;
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
        let vault_options = vault_activity_options(ctx.namespace(), credential_check_options());
        ctx.execute_activity(VaultChecks::check_dispatch_key, (), vault_options)
            .await
            .map_err(activity_failure)?;
        Ok(receipt(&request))
    }
}

boreal_worker::declare_worker! {
    package = "demo-dispatch",
    workflows = {
        export receipt: DispatchWorkflow => "Check the dispatch key and issue a zone receipt",
    },
    configure = |opts| opts.graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE),
}

#[cfg(test)]
mod tests {
    use super::*;
    use boreal_worker::temporalio_common::WorkflowDefinition;
    use demo_core::{Dispatch, Zone};

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
