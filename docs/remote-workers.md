# Central and peripheral workers

Declare the worker once in the shared downstream lab module. Placement does
not create a second Temporal namespace, queue, permissions entry, or schedule.

```nix
boreal.users.alice.workers.greet.placement = {
  central = true;
  peripherals = [ "compute-01" ];
};
```

Set `boreal.role = "central"` on the control-plane host and
`boreal.role = "peripheral"` on compute hosts. Defaults are the original
single-host layout: central role and central-only placement. One host runs at
most one replica of a given worker. The central host keeps the full catalog
and runs any schedule registration locally, even for a peripheral-only worker.
Peripherals run worker services, not Temporal, PostgreSQL, the Boreal SSH shell,
the admin interface, or the updater. Replicas on a queue must have compatible
workflow/activity registrations and receive the same kind of task payloads.

## Fleet check

Pass the real fleet's evaluated NixOS configurations to `boreal.lib.mkFleetCheck`:

```nix
boreal.lib.mkFleetCheck {
  inherit pkgs;
  hosts = { inherit (self.nixosConfigurations) lab compute-01; };
}
```

Put this derivation under `checks.<system>` so `nix flake check` enforces it.
Each key must match `networking.hostName`. A single NixOS evaluation cannot
discover other hosts or enforce exactly one central globally. Validate disposable
test fleets separately from production fleets.

## Remote transport

The central host listens for HTTP/2 over mutual TLS on an explicitly configured
host address. It authenticates each client against the configured CA and an
exact certificate pin for its placement, then routes requests through the same
worker RPC gate used for local replicas. Keep the port on a private interface
or network where practical; Boreal does not add a firewall exception.

The central host configures `boreal.remote.central` with `listenAddress`,
`serverCertificate`, `serverKey`, `clientCaCertificate`, and
`clientCertificatesDirectory`. With the example placement, the directory
contains `alice-greet@compute-01.pem` (the replica's public leaf certificate).
The peripheral configures `boreal.remote.peripheral` with `address`,
`serverCaCertificate`, and `clientCredentialsDirectory` containing
`alice-greet.pem` and `alice-greet.key`. The server certificate must cover the
address, or an optional `serverName` on the peripheral. Credential options are
absolute runtime paths, not Nix path literals or PEM contents. The peripheral
uses systemd credentials to pass the files to the service. Provision only on
machines you trust with task payloads. Application secrets are provisioned
separately, in the peripheral runtime user's own home.

Install the files before starting the services. Replacing a pin or private key
at the same runtime path does not change the Nix configuration: restart the
affected broker or worker to load the new credentials. Enrollment does not
grant access to the SSH command socket or the central updater.

## Updates

The central updater cannot stop a service on another machine. The broker must
fence remote task pickup and account for outstanding poll RPCs and delivered
work before certifying quiescence. It must not bypass the existing Temporal,
PostgreSQL, and lease checks. Keep deployment and recovery centralized; worker
replicas do not make the control plane highly available.

Remote Nexus, eager task delivery, and activity completions by ID are not
supported. Remote task leases are durable and do not expire on a timer: a
disconnected replica with an unsettled task can block an update. See the
[broker notes](../broker/README.md) for what the fence guarantees and what
requires operator investigation.
