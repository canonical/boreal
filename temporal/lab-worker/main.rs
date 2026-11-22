mod activity;
mod model;
mod workflow;

use boreal_sdk::worker;

worker::main! {
    package = "beijing",
    workflows = {
        export workflow::ImageCreate => "Create a synthetic ISO reference",
        export workflow::ImageFlash => "Simulate flashing an ISO to USB media",
        export workflow::ImageDeploy => "Simulate installing an ISO on a DUT",
        export workflow::DutScan => "Refresh the synthetic device inventory",
        export workflow::DutList => "Read the last synthetic device inventory",
        export workflow::DutTest => "Simulate a test plan on one or more DUTs",
        export workflow::DutTestSubmit => "Simulate submitting a test report",
    },
    configure = |opts| opts
        .graceful_shutdown_period(worker::DEFAULT_SHUTDOWN_GRACE)
        .register_activities(activity::Activities::default()),
}
