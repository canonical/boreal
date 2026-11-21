use boreal_sdk::worker;

#[tokio::main]
async fn main() -> worker::Result<()> {
    let worker = std::env::args()
        .nth(1)
        .ok_or("usage: greet <worker-name>")?;
    worker::run(&worker, lab_worker::greet_options).await
}
