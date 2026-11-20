use temporalio_macros::activity_definitions;
use temporalio_sdk::activities::ActivityError;

pub struct Vault;

#[activity_definitions]
impl Vault {
    #[activity(name = "verify-demo-token")]
    pub fn verify_demo_token() -> Result<(), ActivityError> {
        unimplemented!()
    }
}

pub use vault::VerifyDemoToken;
