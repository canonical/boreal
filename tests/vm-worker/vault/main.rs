use temporalio_macros::activities;
use temporalio_sdk::{
    WorkerOptions,
    activities::{ActivityContext, ActivityError},
};

struct Vault;

#[activities]
impl Vault {
    #[activity(definition = lab_worker::VerifyFixtureToken)]
    #[allow(dead_code)] // Callers use the shared definition, not the implementation marker.
    fn verify_fixture_token(_ctx: ActivityContext) -> Result<(), ActivityError> {
        match boreal_worker::read_secret_bytes("api_tok") {
            Ok(token) if token == b"nineteen-byte-token" => Ok(()),
            _ => Err(std::io::Error::other("fixture token validation failed").into()),
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
