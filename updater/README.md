# Boreal updater

This program schedules **and prebuilds** a NixOS configuration. It does **not**
wait for idle, switch configurations, run `nixos-rebuild`, or perform garbage
collection. A successful prebuild ends in `blocked`, not in activation. This is
intentional: Temporal visibility is eventually consistent, and authenticated
clients may submit work without going through a local gate. A zero workflow
count or a local process check cannot establish safe quiescence. There is no
operator override and no simulated switch.

A future activation design must first enforce admission for all relevant
clients and local work, close that gate, establish an authoritative drain
barrier that accounts for in-flight admissions and work, and keep the gate
closed through activation and recovery. The necessary trust boundaries and
operational recovery procedure have not been implemented here.

## Interface

Build with `cargo build --release --manifest-path updater/Cargo.toml`. All
commands must run as root. The binary is `boreal-updater`:

```text
boreal-updater schedule <https-git-repository> <40-digit-commit> <hostname>
boreal-updater status
boreal-updater run
```

For example, after a trusted administrator has chosen and reviewed a revision:

```text
boreal-updater schedule https://github.com/example/lab.git 0123456789abcdef0123456789abcdef01234567 lab-1
boreal-updater status
# Start the configured service, or invoke the worker directly:
boreal-updater run
```

Scheduling writes `/var/lib/boreal-updater/state`, but **does not start a
service**. A new schedule replaces the previous schedule; an operation in
progress is never replaced, and competing schedules/workers fail immediately.
Status may be read while a build is running. The state directory and its parent
must be root-owned, and the state directory must be inaccessible to other users.
State is atomically replaced and synced. Commands report errors on stderr.

`run` resolves the exact Git commit, requires a regular `flake.lock` committed
at the flake root, then builds
`nixosConfigurations.<hostname>.config.system.build.toplevel` with both
`--no-update-lock-file` and `--no-write-lock-file`. Only a deliberately narrow
HTTPS URL syntax without credentials, query, or fragment is accepted; refs,
branch names, and abbreviated hashes are not accepted. The Nix CLI needs Git
flake support, access to the repository and locked inputs, and a trusted `PATH`.
The host name is a single DNS-style label. This is a build target, not a check
that the worker is running on that host. Nix evaluation and builds themselves
may execute work and access the network; only trusted admins should choose the
source. Do not put credentials in the URL or arguments.

After a successful build `status` reports `phase=blocked` and the store path.
The worker prints the reason and exits **nonzero** even on a successful build,
so service success cannot be mistaken for an activated update. Failed builds
are recorded as `failed`; restarting `run` retries a failed or interrupted
`building` state. Re-running a blocked schedule just reports the block. The
service journal contains build errors; they are not stored in the state file.

Each build attempt uses a separate directory under
`/var/lib/boreal-updater/attempt-*` for its Nix result link. This prevents a
surviving child of a killed worker from overwriting a later attempt's link. Nix
normally registers the link as a garbage-collection root. Old attempt
directories and roots are intentionally **not removed**, including on
reschedule or failure; they can accumulate. External garbage collection or
manual removal of roots is outside this program and must be considered separately.

## Service integration

An integrating NixOS module can create a root `Type=oneshot` service whose
`ExecStart` is the absolute store path to `boreal-updater run`, with Nix on a
trusted `PATH` and network access. Keep systemd's `KillMode=control-group` so
stopping the unit terminates the Nix subprocesses as well. A timer or explicit
start can run the worker after scheduling; no timer, unit, or systemd trigger is
installed by this crate. Expect the unit to fail when the prebuild reaches the
intentional block; inspect `status` and the journal rather than treating that as
activation. This service must not run `nixos-rebuild` or GC.
