use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const ISO_PREFIX: &str = "fixture-iso:";
pub const REPORT_PREFIX: &str = "fixture-report:";
const MAX_DUTS: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageInput {
    /// Label for the simulated ISO (letters, digits, hyphens, underscores, or dots).
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlashInput {
    /// Reference returned by image-create.
    pub image_ref: String,
    /// Synthetic removable media target: usb-01 or usb-02.
    pub usb_target: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeployInput {
    /// Reference returned by image-create.
    pub image_ref: String,
    /// Device ID from dut-list.
    pub dut_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TestInput {
    /// Distinct device IDs from dut-list.
    pub dut_ids: Vec<String>,
    /// Name of the Checkbox test plan to simulate.
    pub test_plan: String,
    /// If provided, install this synthetic ISO before testing each device.
    pub image_ref: Option<String>,
    /// Simulate starting Checkbox with no saved sessions.
    #[serde(default)]
    pub clean_sessions: bool,
}

#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    OemShare,
    C3,
    Bughamster,
    BughamsterJira,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitInput {
    /// Reference returned in a dut-test result.
    pub report_ref: String,
    /// Submit to these destinations, without duplicates.
    pub destinations: Vec<Destination>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Dut {
    pub id: String,
    pub model: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Inventory {
    NotScanned,
    Scanned { devices: Vec<Dut> },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TestCase {
    pub dut: Dut,
    pub test_plan: String,
    pub clean_sessions: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Report {
    pub report_ref: String,
    pub verdict: Verdict,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Resolve,
    Deploy,
    Collect,
    Test,
    Package,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Tested { report: Report },
    NotTested { stage: Stage },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DutResult {
    pub dut_id: String,
    pub image_installed: bool,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    pub destination: Destination,
    pub report_ref: String,
    pub simulated: bool,
}

pub fn valid_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(b))
}

pub fn valid_iso(reference: &str) -> bool {
    reference.strip_prefix(ISO_PREFIX).is_some_and(valid_label)
}

pub fn report_ref(run_id: &str, dut_id: &str) -> String {
    format!("{REPORT_PREFIX}{run_id}:{dut_id}")
}

fn valid_report(reference: &str) -> bool {
    reference
        .strip_prefix(REPORT_PREFIX)
        .and_then(|body| body.split_once(':'))
        .is_some_and(|(run, dut)| valid_label(run) && valid_label(dut))
}

impl TestInput {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=MAX_DUTS).contains(&self.dut_ids.len()) {
            return Err("select between 1 and 16 DUTs");
        }
        let mut ids = BTreeSet::new();
        if !self
            .dut_ids
            .iter()
            .all(|id| valid_label(id) && ids.insert(id))
        {
            return Err("DUT IDs must be valid and distinct");
        }
        if self.test_plan.trim().is_empty() || self.test_plan.len() > 128 {
            return Err("supply a non-blank test plan of at most 128 bytes");
        }
        if self
            .image_ref
            .as_deref()
            .is_some_and(|image| !valid_iso(image))
        {
            return Err("expected an image-create fixture reference");
        }
        Ok(())
    }
}

impl SubmitInput {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_report(&self.report_ref) {
            return Err("expected a dut-test fixture report reference");
        }
        let mut destinations = BTreeSet::new();
        if self.destinations.is_empty() || !self.destinations.iter().all(|d| destinations.insert(d))
        {
            return Err("select one or more distinct destinations");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_input_rejects_invalid_selection_plan_and_image() {
        let valid = TestInput {
            dut_ids: vec!["dut-01".into(), "dut-02".into()],
            test_plan: "hardware/audio".into(),
            image_ref: None,
            clean_sessions: false,
        };
        assert!(valid.validate().is_ok());
        for invalid in [
            TestInput {
                dut_ids: vec![],
                ..valid.clone()
            },
            TestInput {
                dut_ids: vec!["dut-01".into(); 2],
                ..valid.clone()
            },
            TestInput {
                dut_ids: vec!["dut-01".into(); 17],
                ..valid.clone()
            },
            TestInput {
                test_plan: "\t".into(),
                ..valid.clone()
            },
            TestInput {
                image_ref: Some("/tmp/os.iso".into()),
                ..valid
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn test_submit_accepts_generated_reference_and_rejects_duplicate_destinations() {
        let mut input = SubmitInput {
            report_ref: report_ref("mock-run", "dut-01"),
            destinations: vec![Destination::C3, Destination::OemShare],
        };
        assert!(input.validate().is_ok());
        input.destinations.push(Destination::C3);
        assert!(input.validate().is_err());
        input.destinations.pop();
        input.report_ref = "fixture-report:../outside:dut-01".into();
        assert!(input.validate().is_err());
    }
}
