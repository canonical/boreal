# Boreal downstream lab starter

Starter flake consuming `github:canonical/boreal`. `boreal.lib.buildRustWorker`
and `boreal.lib.mkDevShell` automatically link `${boreal}/sdk` at
`.boreal/boreal-sdk`, so `flake.lock` is the single source of truth for the
Boreal framework revision.

Develop with the Nix package manager on Linux; the development machine does
not have to run NixOS. Use the QEMU VM for offline workflow execution or to
test deployment behavior. The starter VM targets `x86_64-linux`.

## Quick start

1. Create a new lab project (standalone repository):

   ```sh
   nix flake new my-lab -t github:canonical/boreal
   cd my-lab
   git init && git add .
   ```

   *(Or inside a Boreal checkout as an orphan worktree:
   `git worktree add --orphan -b downstreams/my-lab downstreams/my-lab`, then
   `cd downstreams/my-lab && nix flake init -t ../.. && git add .`.)*

2. Customize workflows and users:
   - Replace or add Temporal worker crates under `temporal/`.
   - Point `boreal.users.<name>.workers.<name>.package` at a worker output.
     `buildRustWorker` runs the `schema` bin and installs `schema.yaml`.
   - Declare lab users, public SSH keys, worker instances, and permissions under
     `boreal.users` in `lab.nix`.

3. Run checks and tests:

   ```sh
   nix flake check
   nix develop
   cargo test --manifest-path temporal/Cargo.toml --workspace --locked
   ```

   *(When testing against local unpushed commits on Boreal's `main` branch, pass
   `--override-input boreal ../.. --no-write-lock-file` to `nix` commands so
   local `file://` paths are not written into `flake.lock`.)*

4. Run workflows locally in the QEMU VM:
   - Set `boreal.vm.devAuthorizedKeys` in `flake.nix` (or `users.<user>.keys` in
     `lab.nix`) to your public SSH key. Build while connected and keep the
     output link, then launch directly (no connection to the lab is needed):

     ```sh
     nix build .#vm --out-link result-vm
     NIX_DISK_IMAGE="$PWD/lab-vm.qcow2" ./result-vm/bin/run-lab-vm
     ```

   - SSH into the guest on `127.0.0.1:2222`:

     ```sh
     ssh -p 2222 alice@127.0.0.1
     # At the Boreal prompt: run example
     # After the workflow finishes, exit the shell before shutting down.
     ssh -p 2222 admin@127.0.0.1 services
     ssh -p 2222 admin@127.0.0.1 poweroff
     ```

5. Deploy and stage updates:
   - Add `./hosts/<hostname>.nix` (importing `hardware-configuration.nix`,
     setting `networking.hostName = "<hostname>";`, and configuring
     `boreal.admin` and `boreal.updater.enable = true;`) and wire
     `nixosConfigurations.<hostname>` in `flake.nix` using `boreal.lib.mkHost`.
   - Clone this repository at `/etc/nixos` on the host tracking your reviewed
     `https://` branch. Commit a remote-resolved `flake.lock` (via
     `nix flake lock` or `nix flake update boreal`), push to the remote branch,
     and run `ssh admin@<host> "updater update"` followed by
     `ssh admin@<host> "updater schedule"`.

**Security note:** Provision user secrets under `~/.config/boreal/secrets`
(`0700` directory, `0600` files) via `boreal-shell` (`secrets set <name>`).
Never place secrets in Git, the Nix store, command-line arguments, or Temporal
history.
