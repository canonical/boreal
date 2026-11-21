use boreal_sdk::worker;

#[tokio::main]
async fn main() -> worker::Result<()> {
    let worker = std::env::args()
        .nth(1)
        .ok_or("usage: count <worker-name>")?;
    worker::run(&worker, lab_worker::count_options).await
}
