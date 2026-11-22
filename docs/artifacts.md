# Share workflow artifacts

Boreal's opt-in Nix artifact support transfers **non-confidential regular
files**. Publish an ISO, a Debian package, or a Git bundle; pass the returned
store path through Temporal instead of the file bytes. The first fetch on a
NixOS host imports the object. Other workers on that host use exactly the same
read-only `/nix/store` path, without another transfer or mount.

## Configure the fleet

Use Nix 2.34 or later. Provision one dedicated, persistent **writable Nix
binary cache** on the central host. Boreal configures clients, not the cache
server. Point all placements at the same backing cache. A `file://` endpoint
uses a directory visible to the worker; an `https://` endpoint must serve the
Nix binary-cache protocol. For uploads it must also accept Nix's HTTP `PUT`
requests. A read-only `nix-serve` endpoint isn't an upload target.

In the shared lab module, opt in the workers:

```nix
boreal.users.alice.workers = {
  image-creation.artifacts = "publish";
  image-deploy.artifacts = "read";
};
```

Keep the package and placement declarations as usual. On central, use the
backing directory directly to avoid opening the central workers' loopback-only
network namespaces:

```nix
boreal.artifacts = {
  readStore = "file:///var/lib/lab-cache";
  writeStore = "file:///var/lib/lab-cache?compression=zstd";
};
systemd.services.boreal-worker-alice-image-creation.serviceConfig.ReadWritePaths = [
  "/var/lib/lab-cache"
];
```

Create the directory before activation. Give publishers filesystem write access
and consumers read access as appropriate; `ReadWritePaths` only adjusts the
service sandbox. On each peripheral, use the endpoints that serve the same
backing cache, for example:

```nix
boreal.artifacts = {
  readStore = "https://artifacts.lab.example";
  writeStore = "https://publish.artifacts.lab.example?compression=zstd";
};
```

Set backend authentication and write policy separately. For example, require
a client certificate for uploads, pass its files to the publisher service
with `LoadCredential`, and use Nix's `tls-certificate` and `tls-private-key`
URL parameters to point at those runtime files. The `read` setting withholds
the SDK's upload endpoint; it is not authorization. Do not put credential
contents in Nix configuration or cache URLs, make workers Nix trusted users,
or disable Nix signature checks.

Files imported into the ordinary host Nix store are readable by other local
users. Do not publish credentials, private source, or images containing
private configuration through this backend.

## Publish, fetch, and retain

Use [`boreal_sdk::artifact`](../sdk/README.md#artifacts) from Activities. Pass
the reference as a string in the workflow result or input. Publishing identical
file bytes returns the same reference; it does not look up a build by its recipe
or skip a build. Nix may need temporary disk space to encode a large upload.

Keep the fetch handle alive until the last reader has finished. Boreal then
releases its local garbage-collection root, so normal operator-managed Nix
collection can reclaim the host copy. Boreal does not enable garbage collection
or run it during deployment. Local collection does not clean a `file://` or
HTTPS binary-cache backend: the operator must retain the authoritative objects
for as long as their references are usable and manage cleanup there separately.
