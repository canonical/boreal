# Boreal `downstreams/demo-lab` deployment

Reference downstream lab consuming `github:canonical/boreal`. This orphan
branch (`downstreams/demo-lab`) lives in gitignored `downstreams/demo-lab`
inside a Boreal checkout. The current lock points to an older local Boreal
revision, so use the override below to get `declare_worker!` and the worker-name
argument. Before deploying from another machine, refresh the lock against a
remote Boreal revision that includes those changes.

Note: `downstreams/demo-lab` shares a disposable demo SSH public key across
`alice`, `bob`, and `admin` for local VM testing; replace those keys with
distinct per-user public keys in any real deployment.

## Development and checks

```sh
nix develop --override-input boreal ../.. --no-write-lock-file
cargo test --manifest-path temporal/Cargo.toml --workspace --locked
nix flake check --override-input boreal ../.. --no-write-lock-file
```

## Run the disposable VM

```sh
mkdir -p "$HOME/boreal-vm"
NIX_DISK_IMAGE="$HOME/boreal-vm/boreal-vm.qcow2" \
  nix run .#boreal-vm --override-input boreal ../.. --no-write-lock-file
```

Connect interactively to `boreal-shell` as `alice` or `bob`:

```sh
ssh -p 2222 alice@127.0.0.1
# Inside boreal-shell:
workers
permissions
run greet alice-run-1 'Alice Smith' 5
status alice-run-1
result alice-run-1
run count alice-run-2 'Élan' 1
secrets set api_token
secrets list
exit
```

Or pipe a secret over stdin without putting secret contents in arguments or
history:

```sh
ssh -p 2222 alice@127.0.0.1 'secrets set api_token' < ./my-token-file
```

Connect to the restricted admin interface (`boreal-admin`) as `admin`:

```sh
ssh -p 2222 admin@127.0.0.1
# Inside boreal-admin:
services
service status temporal
service restart boreal-worker-alice-greet
updater status
poweroff
```
