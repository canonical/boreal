#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    let worker = std::env::args()
        .nth(1)
        .ok_or("usage: greet <worker-name>")?;
    boreal_worker::run_worker(&worker, lab_worker::greet_options).await
}
