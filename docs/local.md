# Local lab

`boreal-local` runs a single-user lab on Linux using rootless Podman. The real
Boreal shell runs on the host. A local Temporal development server, a
user-only broker, and the downstream workers run in separate containers that
share a loopback-only network namespace. The shell uses a private Unix socket
to reach the broker. No ports are published, no images are pulled by Podman,
and the runtime does not use any existing Temporal server, Cloud credentials,
or host Temporal configuration. Nix may need network access to obtain inputs
or build the image the first time, but execution requires no external service.

Use the launcher from the downstream flake root as a regular user with a
working rootless Podman setup, including subordinate UID/GID mappings and a
private `XDG_RUNTIME_DIR`. On NixOS, the standard `virtualisation.podman.enable`
host setup provides the needed Podman integration. The launcher includes a
pinned Podman executable; it does not install or activate host configuration.
Run from a local terminal, not an SSH session.

## Use the shell or run a command directly

```sh
nix develop -c boreal-local
# At the Boreal prompt, `run example` opens the input form.

printf '%s\n' '{"message":"hello"}' |
  nix develop -c boreal-local run example --id local-1 --input-json -
nix develop -c boreal-local status local-1
nix develop -c boreal-local result local-1

nix develop -c boreal-local logs
nix develop -c boreal-local stop
```

With arguments, workflow commands never prompt. As in the SSH shell, `run`
returns after submission; `result` reports when output is available. The
runtime remains available after either command or an interactive shell exits.
`stop` stops only this checkout's containers and keeps local state. Stop the
runtime before switching to a rebuilt worker image; the launcher will not
replace a running lab automatically. Ordinary Temporal replay/versioning rules
still apply to existing workflow executions.

## Wire an existing downstream

The starter template already exposes the command. In an existing flake, add
the following to the per-system package and dev-shell definitions. Pass the
executable **outputs** of `buildRustWorker` and `buildRustVault`, not their
multi-output parent derivations. The `workers` keys are the names shown in the
shell and the suffixes of the `local-<worker>` task queues.

```nix
packages.${system}.boreal-local = boreal.lib.mkLocal pkgs {
  workers.example = self.packages.${system}.example-worker.example;
  # vault = {
  #   package = self.packages.${system}.lab-vault.lab-vault;
  #   secretDir = "./fixtures/alice";
  # };
};

devShells.${system}.default = boreal.lib.mkDevShell pkgs {
  extraPackages = [ self.packages.${system}.boreal-local ];
};
```

`nix run .#boreal-local` is equivalent when running from the same directory.
The launcher uses the configured binary outputs and generated workflow schemas
from that build, so rebuilding requires no duplicate catalog or worker code.

## Persistent state and local secret reads

Each canonical checkout directory has a profile under
`${XDG_STATE_HOME:-$HOME/.local/state}/boreal-local/`. Its identity does not
depend on the commit, generation, or image. Keep the checkout and state base
paths unchanged to reuse a profile. No writable state belongs in the checkout
or Nix store. The runtime keeps Temporal's SQLite database, and the shell
keeps private selector-only history; JSON inputs are not written to shell
history. Temporal does keep workflow input and results as execution data, so
never put secret values there.

To test a vault-backed workflow, configure `vault`, then run `secrets set NAME`
in the local shell (hidden prompt) or pipe fixture bytes to:

```sh
nix develop -c boreal-local secrets set NAME < ./fixture-file
```

Store the fixture file outside the checkout. The command stores it in the
profile's `home/.config/boreal/secrets` by default. To choose another directory,
set `vault.secretDir` to a **quoted relative runtime path**, such as
`"./fixtures/alice"`. It resolves under the profile's `mounts/` directory,
not the checkout. Don't use a Nix path literal: the build must only see the
path string, never the directory contents.

Before starting the runtime with an explicit mount, create its directory:

```sh
secret_dir=$(nix run .#boreal-local -- secret-path)
(umask 077; mkdir -p -- "$secret_dir")
```

`secret-path` doesn't start containers. An explicit path must already exist;
the launcher rejects absolute paths, `..`, symlinks, and directories in the
mount path that aren't owned by the developer with mode `0700`. Secret files
must still use mode `0600`. If you change the mount setting or replace the
directory, stop the runtime before starting it again. The launcher does not
move existing secrets. History stays in the profile home regardless of the
mount path.

Only the vault container mounts the secret directory, read-only at the
UID's actual passwd home. Existing vault activities use the same feature-gated
`boreal_sdk::secret::read` or `read_bytes` API as in a deployment;
there is no environment-variable path override and no host home mount.

Use trusted binaries and test credentials. The local broker does not enforce
the production worker gate or vault activity grants. External-service
activities cannot connect out in this runtime. Use [the local QEMU VM](vm.md)
for deployment behavior and production authorization, including worker
isolation, vault grants, schedules, SSH, and updater drain.

## Smoke test from the framework checkout

On a Linux host with rootless Podman:

```sh
fixture=$(nix build --no-link --print-out-paths .#checks.x86_64-linux.local)
python3 tests/local-smoke.py "$fixture/bin/boreal-local"
```

Use `aarch64-linux` in place of `x86_64-linux` on ARM. The smoke test uses a
throwaway local checkout and profile to exercise interactive and direct
launches, fixture-backed vault reads, and persistence across a restart.
