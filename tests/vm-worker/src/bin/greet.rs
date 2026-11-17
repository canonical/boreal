#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    boreal_worker::run_worker("greet", lab_worker::greet_options).await
}
