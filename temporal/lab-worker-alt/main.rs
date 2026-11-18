#[tokio::main]
async fn main() -> lab_core::LabResult<()> {
    let worker = std::env::args()
        .nth(1)
        .ok_or("usage: lab-worker-alt <worker-name>")?;
    boreal_worker::run_worker(&worker, lab_core::count::worker_options).await
}
