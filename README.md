# Beijing Boreal lab

A synthetic port of the `jenkins-jobs` flows to one central-only Temporal
worker. The `lab-worker` package still deploys as `beijing`. This pass does not
call Jenkins, scan a network, touch devices or USB disks, upload reports, or
write ISO files. Image and report references are fixture strings, not paths to
artifacts.

## Workflows

Use `run beijing --workflow <name>`. The shell and Temporal use the same names.

| Alias | Inputs and behavior |
| --- | --- |
| `image-create` | `label` → synthetic ISO reference. Models the old image-producing `undeploy` jobs. |
| `image-flash` | `image_ref`, `usb_target` (`usb-01` or `usb-02`) → simulated USB flash. |
| `image-deploy` | `image_ref`, `dut_id` → resolve the device, simulate installation, and check readiness. |
| `dut-scan` | `{}` → replace the in-memory inventory with fixture devices. |
| `dut-list` | `{}` → read the last snapshot. Returns `not_scanned` before a scan or after worker restart. |
| `dut-test` | `dut_ids`, `test_plan`, optional `image_ref`, optional `clean_sessions` → one result and report reference per tested device. If an image is given, deploy it first. |
| `dut-test-submit` | `report_ref`, `destinations` → simulated receipts. Valid destinations are `oem_share`, `c3`, `bughamster`, and `bughamster_jira`. |

`dut-test` runs devices in parallel and collects results in input order. The
fixture `dut-fail` returns a failed test verdict with a report; an unknown DUT
is not tested. Neither outcome cancels the other devices. Nothing is submitted
automatically. Run `dut-scan` before the device workflows.

## Run locally

Use Nix and rootless Podman on Linux. Hyphenated workflow names require a
Boreal revision newer than the published revision pinned here. From the
`downstreams/beijing` worktree, use the framework checkout as a temporary
override. Once a revision with alias support is published, update the Boreal
URL in `flake.nix`, refresh `flake.lock`, and drop the override.

```sh
launcher="$(nix build --override-input boreal ../.. --no-write-lock-file \
  --no-link --print-out-paths .#boreal-local)/bin/boreal-local"
"$launcher" run beijing --workflow dut-scan --id scan-1 --input-json '{}'
"$launcher" result scan-1
"$launcher" run beijing --workflow dut-test --id test-1 --input-json \
  '{"dut_ids":["dut-01","dut-fail"],"test_plan":"hardware/audio"}'
"$launcher" result test-1
"$launcher" stop
```

`run` returns after submission. If `result` reports that a workflow is still
running, try it again after completion.

To exercise every alias, including deploy-before-test and report submission,
run the smoke test. It uses and stops a disposable local profile.

```sh
python3 tests/smoke.py "$launcher"
nix flake check --override-input boreal ../.. --no-write-lock-file
nix develop --override-input boreal ../.. --no-write-lock-file \
  -c cargo test --manifest-path temporal/Cargo.toml --workspace --locked
```

For an SSH check on a disposable central host, add your public key to
`boreal.vm.devAuthorizedKeys` and run
`nix run --override-input boreal ../.. --no-write-lock-file .#vm`. Connect as `beijing`
on port `2222` and use the same `run` and `result` commands. There is no
production host definition, no SSH key checked in, and no artifact cache
enabled.
