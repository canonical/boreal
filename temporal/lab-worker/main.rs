mod activity;
mod model;
mod workflow;

use boreal_sdk::worker;

worker::main! {
    package = "beijing",
    workflows = {
        export image_create: workflow::ImageCreate => "Create a synthetic ISO reference",
        export image_flash: workflow::ImageFlash => "Simulate flashing an ISO to USB media",
        export image_deploy: workflow::ImageDeploy => "Simulate installing an ISO on a DUT",
        export dut_scan: workflow::DutScan => "Refresh the synthetic device inventory",
        export dut_list: workflow::DutList => "Read the last synthetic device inventory",
        export dut_test: workflow::DutTest => "Simulate a test plan on one or more DUTs",
        export dut_test_submit: workflow::DutTestSubmit => "Simulate submitting a test report",
    },
    configure = |opts| opts
        .graceful_shutdown_period(worker::DEFAULT_SHUTDOWN_GRACE)
        .register_activities(activity::Activities::default()),
}
