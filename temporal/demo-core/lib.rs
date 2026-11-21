use std::{fmt, io, time::Duration};

use boreal_sdk::worker::vault_task_queue;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use temporalio_common::{HasWorkflowDefinition, RetryPolicy, WorkflowDefinition};
use temporalio_macros::activity_definitions;
use temporalio_sdk::{
    ActivityCloseTimeouts, ActivityExecutionError, ActivityOptions, ApplicationFailure,
    WorkflowTermination, activities::ActivityError,
};

pub const DISPATCH_WORKER: &str = "dispatch";
pub const DISPATCH_WORKFLOW_TYPE: &str = "DispatchWorkflow";

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Name to print on the dispatch receipt. Do not enter a credential here.
    #[schemars(length(min = 1, max = 80))]
    pub requester: String,
    /// Zone that should receive the request.
    pub zone: Zone,
}

impl Request {
    pub fn validate(&self) -> Result<(), WorkflowTermination> {
        if self.requester.trim().is_empty()
            || self.requester.chars().count() > 80
            || self.requester.chars().any(char::is_control)
        {
            return Err(WorkflowTermination::failed_application(
                ApplicationFailure::new(io::Error::other(
                    "requester must be a nonblank name of at most 80 characters without control characters",
                )),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Zone {
    North,
    South,
}

impl fmt::Display for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::North => "north",
            Self::South => "south",
        })
    }
}

/// A client-side marker for starting dispatch and reading its typed result.
pub struct Dispatch;

impl WorkflowDefinition for Dispatch {
    type Input = Request;
    type Output = String;

    fn name(&self) -> &str {
        DISPATCH_WORKFLOW_TYPE
    }
}

impl HasWorkflowDefinition for Dispatch {
    type Run = Self;
}

pub struct VaultChecks;

#[activity_definitions]
impl VaultChecks {
    #[activity(name = "check-desk-badge")]
    pub fn check_desk_badge() -> Result<(), ActivityError> {
        unimplemented!()
    }

    #[activity(name = "check-dispatch-key")]
    pub fn check_dispatch_key() -> Result<(), ActivityError> {
        unimplemented!()
    }
}

pub use vault_checks::{CheckDeskBadge, CheckDispatchKey};

pub fn single_attempt() -> RetryPolicy {
    RetryPolicy::builder().maximum_attempts(1).build()
}

pub fn credential_check_options(namespace: &str) -> ActivityOptions {
    // Missing or unreadable fixtures should fail immediately, not keep retrying.
    ActivityOptions::with_close_timeouts(ActivityCloseTimeouts::ScheduleAndStartToClose {
        schedule_to_close: Duration::from_secs(10),
        start_to_close: Duration::from_secs(3),
    })
    .retry_policy(single_attempt())
    .task_queue(vault_task_queue(namespace))
    .do_not_eagerly_execute(true)
    .build()
}

pub fn activity_failure(error: ActivityExecutionError) -> WorkflowTermination {
    match error {
        ActivityExecutionError::Cancelled(_) => WorkflowTermination::cancelled(),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json, to_value};

    #[test]
    fn request_uses_two_nonsecret_form_fields() {
        for zone in ["north", "south"] {
            let input = json!({"requester": "Alice", "zone": zone});
            let request: Request = from_value(input.clone()).unwrap();
            assert!(request.validate().is_ok());
            assert_eq!(request.zone.to_string(), zone);
            assert_eq!(to_value(request).unwrap(), input);
        }
        for input in [
            json!({"requester": "Alice"}),
            json!({"requester": "Alice", "zone": "east"}),
            json!({"requester": "Alice", "zone": "north", "extra": true}),
        ] {
            assert!(from_value::<Request>(input).is_err());
        }
    }

    #[test]
    fn rejects_blank_or_unprintable_requester_names() {
        for requester in [" ".to_owned(), "Alice\n".to_owned(), "a".repeat(81)] {
            let request = Request {
                requester,
                zone: Zone::North,
            };
            assert!(request.validate().is_err());
        }
    }

    #[test]
    fn test_credential_checks_use_the_namespace_vault_without_eager_execution_or_retries() {
        let options = credential_check_options("alice");
        assert_eq!(options.task_queue.as_deref(), Some("alice-vault"));
        assert!(options.do_not_eagerly_execute);
        assert_eq!(options.retry_policy.unwrap().maximum_attempts(), 1);
        assert!(matches!(
            options.close_timeouts,
            ActivityCloseTimeouts::ScheduleAndStartToClose {
                schedule_to_close,
                start_to_close,
            } if schedule_to_close == Duration::from_secs(10)
                && start_to_close == Duration::from_secs(3)
        ));
    }
}
