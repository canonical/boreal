#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    lab_core::run_worker(lab_core::Workload::Greet).await
}
