#!/usr/bin/env python3
"""Exercise the fixture workflows in an isolated boreal-local profile."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def smoke(launcher, checkout):
    env = {key: value for key, value in os.environ.items() if not key.startswith("SSH_")}
    env["XDG_STATE_HOME"] = str(checkout / "state")

    def cli(*args, data=None):
        command = [launcher, *args]
        result = subprocess.run(
            command, cwd=checkout, env=env, text=True, input=data,
            capture_output=True, timeout=120,
        )
        assert result.returncode == 0, (command, result.stdout, result.stderr)
        return result.stdout

    def workflow(alias, run_id, payload):
        cli("run", "beijing", "--workflow", alias, "--id", run_id,
            "--input-json", "-", data=json.dumps(payload))
        until = time.monotonic() + 45
        while time.monotonic() < until:
            result = cli("result", run_id)
            header, _, body = result.partition("\n")
            if "completed (run ID:" in header:
                return body.strip()
            assert "workflow is still running" in header, result
            time.sleep(.2)
        raise AssertionError(f"timed out waiting for {run_id}")

    try:
        assert json.loads(workflow("dut_list", "empty", {})) == {"status": "not_scanned"}
        scanned = json.loads(workflow("dut_scan", "scan", {}))
        assert scanned["status"] == "scanned"
        assert [device["id"] for device in scanned["devices"]] == ["dut-01", "dut-02", "dut-fail"]
        assert json.loads(workflow("dut_list", "list", {})) == scanned

        image = workflow("image_create", "iso", {"label": "smoke"})
        assert image == "fixture-iso:smoke"
        assert workflow("image_flash", "flash", {"image_ref": image, "usb_target": "usb-01"}) == (
            "Simulated flash of fixture-iso:smoke to usb-01"
        )
        deployed = json.loads(workflow("image_deploy", "install", {
            "image_ref": image, "dut_id": "dut-01",
        }))
        assert deployed["id"] == "dut-01"

        tested = json.loads(workflow("dut_test", "with-install", {
            "image_ref": image, "dut_ids": ["dut-01", "dut-fail"],
            "test_plan": "hardware/audio", "clean_sessions": True,
        }))
        assert [item["image_installed"] for item in tested] == [True, True]
        assert [item["report"]["verdict"] for item in tested] == ["passed", "failed"]
        assert [item["dut_id"] for item in tested] == ["dut-01", "dut-fail"]

        tested_without_image = json.loads(workflow("dut_test", "without-install", {
            "dut_ids": ["unknown", "dut-02"], "test_plan": "sanity",
        }))
        unknown, known = tested_without_image
        assert unknown == {
            "dut_id": "unknown", "status": "not_tested",
            "image_installed": False, "stage": "resolve",
        }
        assert known["image_installed"] is False and known["report"]["verdict"] == "passed"

        report = tested[0]["report"]["report_ref"]
        destinations = ["oem_share", "c3", "bughamster", "bughamster_jira"]
        assert json.loads(workflow("dut_test_submit", "submit", {
            "report_ref": report, "destinations": destinations,
        })) == [
            {"destination": destination, "report_ref": report, "simulated": True}
            for destination in destinations
        ]
    finally:
        cli("stop")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: python3 tests/smoke.py BOREAL-LOCAL")
    preferred = Path("/tmp/opencode")
    with tempfile.TemporaryDirectory(prefix="beijing-smoke-", dir=preferred if preferred.is_dir() else None) as tmp:
        smoke(Path(sys.argv[1]).resolve(strict=True), Path(tmp))
    print("Beijing workflow smoke test passed")
