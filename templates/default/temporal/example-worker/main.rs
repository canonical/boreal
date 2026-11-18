#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    let worker = std::env::args()
        .nth(1)
        .ok_or("usage: example <worker-name>")?;
    boreal_worker::run_worker(&worker, example_worker::worker_options).await
}
