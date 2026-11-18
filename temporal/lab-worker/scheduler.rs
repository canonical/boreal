#[tokio::main]
async fn main() -> lab_core::LabResult<()> {
    let worker = std::env::args()
        .nth(1)
        .ok_or("usage: scheduler <worker-name>")?;
    lab_core::register_greet_schedule(&worker).await
}
