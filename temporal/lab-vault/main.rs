use temporalio_macros::activities;
use temporalio_sdk::{
    WorkerOptions,
    activities::{ActivityContext, ActivityError},
};

struct Vault;

#[activities]
impl Vault {
    #[activity(definition = lab_core::vault::VerifyDemoToken)]
    #[allow(dead_code)] // Callers use the shared definition, not the implementation marker.
    fn verify_demo_token(_ctx: ActivityContext) -> Result<(), ActivityError> {
        match boreal_worker::read_secret_bytes("api_token") {
            Ok(token) if !token.is_empty() => Ok(()),
            _ => Err(std::io::Error::other("demo token unavailable").into()),
        }
    }
}

#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    boreal_worker::run_worker("vault", |queue| {
        Ok(WorkerOptions::new(queue).register_activities(Vault).build())
    })
    .await
}
