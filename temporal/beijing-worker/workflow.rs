use std::{io, time::Duration};

use futures::future::{self, Either};
use temporalio_macros::{workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, ApplicationFailure, ChildWorkflowOptions, WorkflowContext, WorkflowResult,
    WorkflowTermination,
};

use crate::{
    activity::Activities,
    model::{
        self, DeployInput, Destination, Dut, DutResult, Empty, FlashInput, ImageInput, Inventory,
        Outcome, Receipt, Report, Stage, SubmitInput, TestCase, TestInput,
    },
};

fn rejected(message: &'static str) -> WorkflowTermination {
    ApplicationFailure::new(io::Error::new(io::ErrorKind::InvalidInput, message)).into()
}

fn options() -> ActivityOptions {
    ActivityOptions::with_schedule_to_close_timeout(Duration::from_secs(20)).build()
}

#[workflow]
#[derive(Default)]
pub struct ImageCreate;

#[workflow_methods]
impl ImageCreate {
    #[run(name = "image-create")]
    pub async fn run(ctx: &mut WorkflowContext<Self>, input: ImageInput) -> WorkflowResult<String> {
        if !model::valid_label(&input.label) {
            return Err(rejected("invalid ISO fixture label"));
        }
        Ok(ctx
            .execute_activity(Activities::create_iso, input, options())
            .await?)
    }
}

#[workflow]
#[derive(Default)]
pub struct ImageFlash;

#[workflow_methods]
impl ImageFlash {
    #[run(name = "image-flash")]
    pub async fn run(ctx: &mut WorkflowContext<Self>, input: FlashInput) -> WorkflowResult<String> {
        if !model::valid_iso(&input.image_ref)
            || !matches!(input.usb_target.as_str(), "usb-01" | "usb-02")
        {
            return Err(rejected(
                "expected a fixture ISO reference and usb-01 or usb-02",
            ));
        }
        Ok(ctx
            .execute_activity(Activities::flash_usb, input, options())
            .await?)
    }
}

#[workflow]
#[derive(Default)]
pub struct ImageDeploy;

#[workflow_methods]
impl ImageDeploy {
    #[run(name = "image-deploy")]
    pub async fn run(ctx: &mut WorkflowContext<Self>, input: DeployInput) -> WorkflowResult<Dut> {
        if !model::valid_iso(&input.image_ref) || !model::valid_label(&input.dut_id) {
            return Err(rejected(
                "expected a fixture ISO reference and valid DUT ID",
            ));
        }
        let dut = ctx
            .execute_activity(Activities::resolve_dut, input.dut_id.clone(), options())
            .await?
            .ok_or_else(|| rejected("unknown DUT; run dut-scan and check dut-list"))?;
        ctx.execute_activity(Activities::install_iso, input, options())
            .await?;
        ctx.execute_activity(Activities::check_ready, dut.clone(), options())
            .await?;
        Ok(dut)
    }
}

#[workflow]
#[derive(Default)]
pub struct DutScan;

#[workflow_methods]
impl DutScan {
    #[run(name = "dut-scan")]
    pub async fn run(ctx: &mut WorkflowContext<Self>, _: Empty) -> WorkflowResult<Inventory> {
        Ok(ctx
            .execute_activity(Activities::scan_duts, (), options())
            .await?)
    }
}

#[workflow]
#[derive(Default)]
pub struct DutList;

#[workflow_methods]
impl DutList {
    #[run(name = "dut-list")]
    pub async fn run(ctx: &mut WorkflowContext<Self>, _: Empty) -> WorkflowResult<Inventory> {
        Ok(ctx
            .execute_activity(Activities::list_duts, (), options())
            .await?)
    }
}

#[workflow]
#[derive(Default)]
pub struct DutTest;

#[workflow_methods]
impl DutTest {
    #[run(name = "dut-test")]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
        input: TestInput,
    ) -> WorkflowResult<Vec<DutResult>> {
        input.validate().map_err(rejected)?;
        let tests = future::join_all(
            input
                .dut_ids
                .iter()
                .enumerate()
                .map(|(index, dut_id)| test_dut(ctx, &input, index, dut_id)),
        );
        let cancelled = ctx.cancelled();
        futures::pin_mut!(cancelled, tests);
        match future::select(cancelled, tests).await {
            Either::Left(_) => Err(WorkflowTermination::cancelled()),
            Either::Right((results, _)) => Ok(results),
        }
    }
}

async fn test_dut(
    ctx: &WorkflowContext<DutTest>,
    input: &TestInput,
    index: usize,
    dut_id: &str,
) -> DutResult {
    let mut image_installed = false;
    let result = async {
        let dut = if let Some(image_ref) = &input.image_ref {
            let deploy_id = format!("beijing-deploy-{}-{index}", ctx.run_id());
            let dut = ctx
                .start_child_workflow(
                    ImageDeploy::run,
                    DeployInput {
                        dut_id: dut_id.to_owned(),
                        image_ref: image_ref.clone(),
                    },
                    ChildWorkflowOptions::workflow_id(deploy_id),
                )
                .await
                .map_err(|_| Stage::Deploy)?
                .result()
                .await
                .map_err(|_| Stage::Deploy)?;
            image_installed = true;
            dut
        } else {
            ctx.execute_activity(Activities::resolve_dut, dut_id.to_owned(), options())
                .await
                .map_err(|_| Stage::Resolve)?
                .ok_or(Stage::Resolve)?
        };
        let dut = ctx
            .execute_activity(Activities::collect_device_metadata, dut, options())
            .await
            .map_err(|_| Stage::Collect)?;
        let case = TestCase {
            dut,
            test_plan: input.test_plan.clone(),
            clean_sessions: input.clean_sessions,
        };
        let verdict = ctx
            .execute_activity(Activities::run_test_plan, case, options())
            .await
            .map_err(|_| Stage::Test)?;
        let report = Report {
            report_ref: model::report_ref(ctx.run_id(), dut_id),
            verdict,
        };
        ctx.execute_activity(Activities::package_checkbox_report, report, options())
            .await
            .map_err(|_| Stage::Package)
    }
    .await;
    let outcome = match result {
        Ok(report) => Outcome::Tested { report },
        Err(stage) => Outcome::NotTested { stage },
    };
    DutResult {
        dut_id: dut_id.to_owned(),
        image_installed,
        outcome,
    }
}

#[workflow]
#[derive(Default)]
pub struct DutTestSubmit;

#[workflow_methods]
impl DutTestSubmit {
    #[run(name = "dut-test-submit")]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
        input: SubmitInput,
    ) -> WorkflowResult<Vec<Receipt>> {
        input.validate().map_err(rejected)?;
        future::join_all(
            input
                .destinations
                .iter()
                .map(|destination| submit(ctx, *destination, &input.report_ref)),
        )
        .await
        .into_iter()
        .collect()
    }
}

async fn submit(
    ctx: &WorkflowContext<DutTestSubmit>,
    destination: Destination,
    report: &str,
) -> WorkflowResult<Receipt> {
    let report = report.to_owned();
    Ok(match destination {
        Destination::OemShare => {
            ctx.execute_activity(Activities::submit_oem_share, report, options())
                .await?
        }
        Destination::C3 => {
            ctx.execute_activity(Activities::submit_c3, report, options())
                .await?
        }
        Destination::Bughamster => {
            ctx.execute_activity(Activities::submit_bughamster, report, options())
                .await?
        }
        Destination::BughamsterJira => {
            ctx.execute_activity(Activities::submit_bughamster_jira, report, options())
                .await?
        }
    })
}
