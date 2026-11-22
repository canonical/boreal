use std::{
    io,
    sync::{Arc, Mutex},
};

use temporalio_macros::activities;
use temporalio_sdk::activities::{ActivityContext, ActivityError};

use crate::model::{self, Destination, Dut, Inventory, Receipt, Report, TestCase, Verdict};

#[derive(Default)]
pub struct Activities {
    devices: Mutex<Option<Vec<Dut>>>,
}

fn fixture_devices() -> Vec<Dut> {
    [
        ("dut-01", "fixture-laptop"),
        ("dut-02", "fixture-desktop"),
        ("dut-fail", "fixture-failure"),
    ]
    .into_iter()
    .map(|(id, model)| Dut {
        id: id.into(),
        model: model.into(),
    })
    .collect()
}

impl Activities {
    fn refresh(&self) -> Result<Inventory, ActivityError> {
        let devices = fixture_devices();
        *self
            .devices
            .lock()
            .map_err(|_| io::Error::other("inventory lock poisoned"))? = Some(devices.clone());
        Ok(Inventory::Scanned { devices })
    }

    fn snapshot(&self) -> Result<Inventory, ActivityError> {
        Ok(self
            .devices
            .lock()
            .map_err(|_| io::Error::other("inventory lock poisoned"))?
            .as_ref()
            .map_or(Inventory::NotScanned, |devices| Inventory::Scanned {
                devices: devices.clone(),
            }))
    }
}

#[activities]
impl Activities {
    #[activity]
    pub async fn create_iso(
        _ctx: ActivityContext,
        input: model::ImageInput,
    ) -> Result<String, ActivityError> {
        Ok(format!("{}{label}", model::ISO_PREFIX, label = input.label))
    }

    #[activity]
    pub async fn flash_usb(
        _ctx: ActivityContext,
        input: model::FlashInput,
    ) -> Result<String, ActivityError> {
        Ok(format!(
            "Simulated flash of {} to {}",
            input.image_ref, input.usb_target
        ))
    }

    #[activity]
    pub async fn scan_duts(
        self: Arc<Self>,
        _ctx: ActivityContext,
    ) -> Result<Inventory, ActivityError> {
        self.refresh()
    }

    #[activity]
    pub async fn list_duts(
        self: Arc<Self>,
        _ctx: ActivityContext,
    ) -> Result<Inventory, ActivityError> {
        self.snapshot()
    }

    #[activity]
    pub async fn resolve_dut(
        self: Arc<Self>,
        _ctx: ActivityContext,
        dut_id: String,
    ) -> Result<Option<Dut>, ActivityError> {
        Ok(match self.snapshot()? {
            Inventory::NotScanned => None,
            Inventory::Scanned { devices } => devices.into_iter().find(|dut| dut.id == dut_id),
        })
    }

    #[activity]
    pub async fn install_iso(
        _ctx: ActivityContext,
        _input: model::DeployInput,
    ) -> Result<(), ActivityError> {
        Ok(())
    }

    #[activity]
    pub async fn check_ready(_ctx: ActivityContext, _dut: Dut) -> Result<(), ActivityError> {
        Ok(())
    }

    #[activity]
    pub async fn collect_device_metadata(
        _ctx: ActivityContext,
        dut: Dut,
    ) -> Result<Dut, ActivityError> {
        Ok(dut)
    }

    #[activity]
    pub async fn run_test_plan(
        _ctx: ActivityContext,
        case: TestCase,
    ) -> Result<Verdict, ActivityError> {
        Ok(if case.dut.id == "dut-fail" {
            Verdict::Failed
        } else {
            Verdict::Passed
        })
    }

    #[activity]
    pub async fn package_checkbox_report(
        _ctx: ActivityContext,
        report: Report,
    ) -> Result<Report, ActivityError> {
        Ok(report)
    }

    #[activity]
    pub async fn submit_oem_share(
        _ctx: ActivityContext,
        report_ref: String,
    ) -> Result<Receipt, ActivityError> {
        Ok(receipt(Destination::OemShare, report_ref))
    }

    #[activity]
    pub async fn submit_c3(
        _ctx: ActivityContext,
        report_ref: String,
    ) -> Result<Receipt, ActivityError> {
        Ok(receipt(Destination::C3, report_ref))
    }

    #[activity]
    pub async fn submit_bughamster(
        _ctx: ActivityContext,
        report_ref: String,
    ) -> Result<Receipt, ActivityError> {
        Ok(receipt(Destination::Bughamster, report_ref))
    }

    #[activity]
    pub async fn submit_bughamster_jira(
        _ctx: ActivityContext,
        report_ref: String,
    ) -> Result<Receipt, ActivityError> {
        Ok(receipt(Destination::BughamsterJira, report_ref))
    }
}

fn receipt(destination: Destination, report_ref: String) -> Receipt {
    Receipt {
        destination,
        report_ref,
        simulated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_replaces_snapshot_and_list_does_not_scan() {
        let activities = Activities::default();
        assert_eq!(activities.snapshot().unwrap(), Inventory::NotScanned);
        let scan = activities.refresh().unwrap();
        assert!(matches!(&scan, Inventory::Scanned { devices } if devices.len() == 3));
        assert_eq!(activities.refresh().unwrap(), scan);
        assert_eq!(activities.snapshot().unwrap(), scan);
    }
}
