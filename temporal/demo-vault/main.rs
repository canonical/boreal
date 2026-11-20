use std::io;

use temporalio_macros::activities;
use temporalio_sdk::{
    WorkerOptions,
    activities::{ActivityContext, ActivityError},
};

struct Vault;

fn require_nonempty(
    credential: boreal_worker::Result<Vec<u8>>,
    message: &'static str,
) -> Result<(), ActivityError> {
    match credential {
        Ok(bytes) if !bytes.is_empty() => Ok(()),
        // Report neither the bytes nor the underlying filesystem error.
        _ => Err(io::Error::other(message).into()),
    }
}

#[activities]
impl Vault {
    #[activity(definition = demo_core::CheckDeskBadge)]
    #[allow(dead_code)] // Callers use the shared definition, not the implementation marker.
    fn check_desk_badge(_ctx: ActivityContext) -> Result<(), ActivityError> {
        require_nonempty(
            boreal_worker::read_secret_bytes("desk_badge"),
            "desk_badge is missing, empty, or unreadable",
        )
    }

    #[activity(definition = demo_core::CheckDispatchKey)]
    #[allow(dead_code)]
    fn check_dispatch_key(_ctx: ActivityContext) -> Result<(), ActivityError> {
        require_nonempty(
            boreal_worker::read_secret_bytes("dispatch_key"),
            "dispatch_key is missing, empty, or unreadable",
        )
    }
}

#[tokio::main]
async fn main() -> boreal_worker::Result<()> {
    boreal_worker::run_worker("vault", |queue| {
        Ok(WorkerOptions::new(queue)
            .graceful_shutdown_period(boreal_worker::DEFAULT_SHUTDOWN_GRACE)
            .register_activities(Vault)
            .build())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_nonempty_bytes_and_sanitizes_errors() {
        let message = "credential unavailable";
        assert!(require_nonempty(Ok(vec![0xff]), message).is_ok());
        for value in [Ok(vec![]), Err(io::Error::other("private detail").into())] {
            let error = require_nonempty(value, message).unwrap_err();
            let error = format!("{error:?}");
            assert!(error.contains(message));
            assert!(!error.contains("private detail"));
        }
    }
}
