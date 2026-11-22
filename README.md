# Beijing Boreal lab

A synthetic port of the `jenkins-jobs` flows to one central-only Temporal
worker. This pass does not call Jenkins, scan a network, touch devices or USB
disks, upload reports, or write ISO files. Image and report references are
fixture strings, not paths to artifacts.

## Workflows

Use `run beijing --workflow <alias>`. Boreal shell aliases use underscores;
the registered Temporal workflow types use hyphens.

| Alias | Inputs and behavior |
| --- | --- |
| `image_create` | `label` → synthetic ISO reference. Models the old image-producing `undeploy` jobs. |
| `image_flash` | `image_ref`, `usb_target` (`usb-01` or `usb-02`) → simulated USB flash. |
| `image_deploy` | `image_ref`, `dut_id` → resolve the device, simulate installation, and check readiness. |
| `dut_scan` | `{}` → replace the in-memory inventory with fixture devices. |
| `dut_list` | `{}` → read the last snapshot. Returns `not_scanned` before a scan or after worker restart. |
| `dut_test` | `dut_ids`, `test_plan`, optional `image_ref`, optional `clean_sessions` → one result and report reference per tested device. If an image is given, deploy it first. |
| `dut_test_submit` | `report_ref`, `destinations` → simulated receipts. Valid destinations are `oem_share`, `c3`, `bughamster`, and `bughamster_jira`. |

`dut_test` runs devices in parallel and collects results in input order. The
fixture `dut-fail` returns a failed test verdict with a report; an unknown DUT
is not tested. Neither outcome cancels the other devices. Nothing is submitted
automatically. Run `dut_scan` before the device workflows.

## Run locally

Use Nix and rootless Podman on Linux. For example:

```sh
nix run .#boreal-local -- run beijing --workflow dut_scan --id scan-1 --input-json '{}'
nix run .#boreal-local -- result scan-1
nix run .#boreal-local -- run beijing --workflow dut_test --id test-1 --input-json \
  '{"dut_ids":["dut-01","dut-fail"],"test_plan":"hardware/audio"}'
nix run .#boreal-local -- result test-1
nix run .#boreal-local -- stop
```

`run` returns after submission. If `result` reports that a workflow is still
running, try it again after completion.

To exercise every alias, including deploy-before-test and report submission,
run the smoke test. It uses and stops a disposable local profile.

```sh
python3 tests/smoke.py "$(nix build --no-link --print-out-paths .#boreal-local)/bin/boreal-local"
nix flake check
nix develop -c cargo test --manifest-path temporal/Cargo.toml --workspace --locked
```

For an SSH check on a disposable central host, add your public key to
`boreal.vm.devAuthorizedKeys` and run `nix run .#vm`. Connect as `beijing`
on port `2222` and use the same `run` and `result` commands. There is no
production host definition, no SSH key checked in, and no artifact cache
enabled.
