#[tokio::main]
async fn main() -> lab_core::LabResult<()> {
    let worker = std::env::args()
        .nth(1)
        .ok_or("usage: lab-worker <worker-name>")?;
    boreal_worker::run_worker(&worker, lab_core::greet::worker_options).await
}
