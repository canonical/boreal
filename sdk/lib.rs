//! Downstream integration with Boreal, using native Temporal workflows and activities.

pub mod artifact;
pub mod client;
#[cfg(feature = "schema")]
pub mod schema;
#[cfg(all(unix, feature = "read-secrets"))]
pub mod secret;
pub mod worker;

type Error = Box<dyn std::error::Error>;
