die() {
    printf 'boreal-local: %s\n' "$*" >&2
    exit 1
}

case ${1-} in
    -h|--help)
        printf '%s\n' 'Usage: boreal-local [run|list|status|result|workers|secrets set ...]' \
            '       boreal-local [stop|logs|secret-path]' \
            'With no arguments, open the interactive local Boreal shell.'
        exit 0 ;;
    stop|logs|secret-path)
        [[ $# == 1 ]] || die "usage: boreal-local $1" ;;
esac

[[ $UID != 0 ]] || die 'run this as a regular user with rootless Podman'
[[ -z ${SSH_CONNECTION-}${SSH_CLIENT-}${SSH_TTY-}${SSH_ORIGINAL_COMMAND-} ]] \
    || die 'local mode is not available in an SSH session'
[[ $# != 0 || ( -t 0 && -t 1 ) ]] || die 'interactive mode requires a terminal'
umask 077

verify_private_dir() {
    [[ -d $1 && ! -L $1 && $(stat -c '%u:%a' -- "$1") == "$UID:700" ]] \
        || die "expected a private, owned 0700 directory: $1"
}

private_dir() {
    if [[ ! -e $1 && ! -L $1 ]]; then
        mkdir -m 700 -- "$1" || die "cannot create $1"
    fi
    verify_private_dir "$1"
}

# Checkout identity does not depend on the commit, the flake lock or the image.
checkout=$(pwd -P)
key=$(printf '%s' "$checkout" | sha256sum)
key=${key:0:24}
safe_base() {
    case $1/ in
        /nix/store/*|"$checkout/"*|*:*) die "unsafe state or runtime path: $1" ;;
    esac
}
state_base=$(realpath -m -- "${XDG_STATE_HOME:-$HOME/.local/state}")
safe_base "$state_base"
name=boreal-local-$key
profile=$state_base/boreal-local/$key
home=$profile/home
catalog=$profile/catalog
secrets=$home/.config/boreal/secrets
if [[ -n $vault_secret_rel ]]; then
    secrets=$profile/mounts/$vault_secret_rel
fi
if [[ ${1-} == secret-path ]]; then
    printf '%s\n' "$secrets"
    exit 0
fi

[[ -n ${XDG_RUNTIME_DIR-} ]] || die 'XDG_RUNTIME_DIR is required for rootless Podman'
runtime_base=$(realpath -e -- "$XDG_RUNTIME_DIR")
safe_base "$runtime_base"
ipc=$runtime_base/$name
socket=$ipc/broker.sock
[[ $(printf '%s' "$socket" | wc -c) -lt 100 ]] || die 'XDG_RUNTIME_DIR is too long for the broker socket'

[[ -d $runtime_base ]] || die 'XDG_RUNTIME_DIR is not a directory'
private_dir "$runtime_base"
mkdir -p -- "$state_base"
private_dir "$state_base/boreal-local"
private_dir "$profile"
[[ ! -L $profile/lock ]] || die 'unsafe local runtime lock'
exec 9>"$profile/lock"
flock -x 9

podman() {
    # Never use a remote Podman service or inject host subscription/secret mounts.
    "$podman_bin" --remote=false --default-mounts-file=/dev/null "$@" 9>&- </dev/null
}

label=dev.boreal.profile=$key
build_label=dev.boreal.image
temporal=$name-temporal
broker=$name-broker
expected=("$temporal" "$broker")
for worker in "${worker_names[@]}"; do
    expected+=("$name-worker-$worker")
done
[[ -z $vault_bin ]] || expected+=("$name-vault")

containers() {
    podman ps -a --filter "label=$label" --format '{{.Names}}'
}

stop_runtime() {
    local names item have_temporal=0
    names=$(containers) || return 1
    [[ -n $names ]] || return 0
    # Stop pollers and the broker before removing the Temporal network namespace.
    while IFS= read -r item; do
        if [[ $item == "$temporal" ]]; then
            have_temporal=1
            continue
        fi
        podman stop --ignore --time 35 "$item" >/dev/null || return 1
        podman rm --ignore "$item" >/dev/null || return 1
    done <<< "$names"
    if (( have_temporal )); then
        podman stop --ignore --time 10 "$temporal" >/dev/null || return 1
        podman rm --ignore "$temporal" >/dev/null || return 1
    fi
}

case ${1-} in
    stop) stop_runtime; exit ;;
    logs)
        names=$(containers)
        [[ -n $names ]] || die 'no local containers'
        while IFS= read -r item; do
            printf '\n== %s ==\n' "$item"
            podman logs --tail 100 "$item"
        done <<< "$names"
        exit ;;
esac

private_dir "$home"
private_dir "$profile/temporal"
private_dir "$profile/broker"
private_dir "$ipc"

shell_options=()
vault_mount_id=
if [[ -n $vault_secret_rel ]]; then
    # Validate each parent, rather than letting a symlink escape the profile.
    mount_dir=$profile/mounts
    verify_private_dir "$mount_dir"
    IFS=/ read -r -a parts <<< "$vault_secret_rel"
    for part in "${parts[@]}"; do
        mount_dir+=/$part
        verify_private_dir "$mount_dir"
    done
    # An existing bind mount still points to the original directory after a rename.
    vault_mount_id=$vault_secret_rel:$(stat -c '%d:%i' -- "$secrets")
    shell_options=(--secret-dir "$secrets")
fi

start_container() {
    local container=$1 network=$2
    shift 2
    podman run --detach --name "$container" --label "$label" \
        --label "$build_label=$image_ref" --label "dev.boreal.vault-mount=$vault_mount_id" \
        --network "$network" \
        --pull=never --userns=keep-id --user "$UID:$gid" --pid=private --ipc=private \
        --sdnotify=ignore --restart=no --passwd=true \
        --passwd-entry "local:x:$UID:$gid::/home/local:/sbin/nologin" \
        --read-only --cap-drop=ALL --security-opt=no-new-privileges \
        --http-proxy=false --unsetenv-all --env HOME=/home/local \
        --env TEMPORAL_ADDRESS=127.0.0.1:7233 --env TEMPORAL_NAMESPACE=local \
        "$@" >/dev/null
}

temporal_ready() {
    podman exec "$temporal" "$temporal_bin" operator namespace describe \
        --address 127.0.0.1:7233 --namespace local --disable-config-file \
        --disable-config-env --command-timeout 1s --output none >/dev/null 2>&1
}

await_ready() {
    local what=$1 tries=$2
    shift 2
    while ! "$@"; do
        (( tries -= 1 )) || die "timed out waiting for $what; see boreal-local logs"
        sleep .25
    done
}

launch() {
    gid=$(id -g)
    if ! podman image exists "$image_ref"; then
        podman load --signature-policy "$archive_policy" --input "$archive" >&2
    fi
    if [[ -e $socket || -L $socket ]]; then
        [[ -S $socket && ! -L $socket && -O $socket ]] || die 'unsafe stale broker socket'
        rm -- "$socket"
    fi
    # The catalog loader requires private modes; Nix normalizes store paths.
    private_dir "$catalog"
    private_dir "$catalog/users"
    private_dir "$catalog/users/local"
    install -m 400 -- "$store_catalog/schemas.yaml" "$catalog/schemas.yaml"
    install -m 400 -- "$store_catalog/users/local/perms.yaml" "$catalog/users/local/perms.yaml"
    start_container "$temporal" none --volume "$profile/temporal:/data:Z" \
        "$image_ref" "$temporal_bin" server start-dev --headless --ip 127.0.0.1 \
        --namespace local --db-filename /data/temporal.sqlite
    await_ready 'the local Temporal namespace' 80 temporal_ready
    start_container "$broker" "container:$temporal" --volume "$profile/broker:/data:Z" \
        --volume "$ipc:/ipc:Z" --volume "$catalog:/catalog:ro,Z" \
        "$image_ref" "$broker_bin" local /catalog /ipc/broker.sock /data
    if [[ -n $vault_bin ]]; then
        # Match the vault account's passwd home. No other container gets this mount.
        if [[ -z $vault_secret_rel ]]; then
            private_dir "$home/.config"
            private_dir "$home/.config/boreal"
            private_dir "$secrets"
        fi
        start_container "$name-vault" "container:$temporal" \
            --volume "$secrets:/home/local/.config/boreal/secrets:ro,Z" \
            "$image_ref" "$vault_bin"
    fi
    for i in "${!worker_names[@]}"; do
        worker=${worker_names[i]}
        start_container "$name-worker-$worker" "container:$temporal" \
            "$image_ref" "${worker_bins[i]}" "$worker"
    done
    await_ready 'the local broker socket' 80 test -S "$socket"
}

check_runtime() {
    local item="${1-}" info
    info=$(podman inspect --type container --format \
        '{{.State.Running}} {{index .Config.Labels "dev.boreal.image"}} {{index .Config.Labels "dev.boreal.vault-mount"}}' "$item")
    [[ $info == "true $image_ref $vault_mount_id" ]]
}

names=$(containers)
if [[ -z $names ]]; then
    cleanup_start() {
        if [[ -n $(containers) ]]; then
            printf '%s\n' 'boreal-local: local startup failed; recent container logs:' >&2
            while IFS= read -r item; do
                podman logs --tail 12 "$item" >&2 || true
            done < <(containers)
        fi
        stop_runtime || printf '%s\n' 'boreal-local: run boreal-local stop to remove partial startup' >&2
    }
    trap cleanup_start EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    launch
else
    [[ $(wc -l <<< "$names") == "${#expected[@]}" ]] || die 'local runtime changed or incomplete; run boreal-local stop first'
fi
for item in "${expected[@]}"; do
    check_runtime "$item" || die 'local runtime changed or a container exited; run boreal-local logs or stop'
done
[[ -S $socket && ! -L $socket && -O $socket ]] || die 'local broker socket unavailable; run boreal-local stop first'
trap - EXIT INT TERM
exec 9>&-
exec "$shell_bin" --local "$home" "$catalog" "$socket" "${shell_options[@]}" "$@"
