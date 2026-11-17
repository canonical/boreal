#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    boreal_worker::run_worker("count", lab_worker::count_options).await
}
