#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    let worker = std::env::args()
        .nth(1)
        .ok_or("usage: count <worker-name>")?;
    boreal_worker::run_worker(&worker, lab_worker::count_options).await
}
