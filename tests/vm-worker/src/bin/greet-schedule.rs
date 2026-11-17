use std::time::Duration;

use boreal_worker::temporalio_client::schedules::{
    CreateScheduleOptions, ScheduleAction, ScheduleError, ScheduleOverlapPolicy, ScheduleSpec,
};
use lab_worker::{DemoInput, DemoWorkflow};

const SCHEDULE_ID: &str = "greet-30s";

fn already_exists(err: &ScheduleError) -> bool {
    match err {
        ScheduleError::Rpc(status) => status.code() as i32 == 6,
        _ => false,
    }
}

#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    let client = boreal_worker::connect().await?;
    let queue = boreal_worker::task_queue("greet")?;
    let spec = ScheduleSpec::from_interval(Duration::from_secs(30));
    let input = DemoInput {
        name: "Scheduler".into(),
        wait_seconds: 1,
    };
    let action = ScheduleAction::start_workflow(
        DemoWorkflow::scheduled(input.clone()),
        input,
        queue,
        SCHEDULE_ID,
    );
    let opts = CreateScheduleOptions::builder()
        .action(action.clone())
        .spec(spec.clone())
        .overlap_policy(ScheduleOverlapPolicy::Skip)
        .build();
    match client.create_schedule(SCHEDULE_ID, opts).await {
        Ok(_) => Ok(()),
        Err(err) if already_exists(&err) => client
            .get_schedule_handle(SCHEDULE_ID)
            .update(
                move |update| {
                    update
                        .set_spec(spec)
                        .set_action(action)
                        .set_paused(false)
                        .set_overlap_policy(ScheduleOverlapPolicy::Skip)
                        .set_catchup_window(Duration::from_secs(60));
                },
                Default::default(),
            )
            .await
            .map_err(Into::into),
        Err(err) => Err(err.into()),
    }
}
