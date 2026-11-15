# Boreal `downstreams/demo-lab` deployment

Reference downstream lab consuming `github:canonical/boreal`. This orphan
branch (`downstreams/demo-lab`) lives in gitignored `downstreams/demo-lab`
inside a Boreal checkout (or standalone with
`--override-input boreal <url> --no-write-lock-file`; ensure committed
`flake.lock` files resolve remote URLs rather than local `file://` overrides
before staging updates with `boreal-updater`).

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
start greet alice-run-1 'Alice Smith' 5
status alice-run-1
result alice-run-1
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
update status
poweroff
```
