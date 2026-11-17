#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    boreal_worker::run_worker("example", example_worker::worker_options).await
}
