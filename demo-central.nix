{
  self,
  demo,
  publicKey,
  pkgs,
  ...
}:

let
  veth = "ve-compute-01";
  pkiDirectory = "/run/boreal-demo-pki";
  pki = "${pkiDirectory}/ready";
  pkiUnit = "boreal-demo-pki.service";
  containerUnit = "container@compute-01.service";
in
{
  networking = {
    hostName = "boreal-vm";
    nat.enable = false;
    firewall.interfaces.${veth}.allowedTCPPorts = [ demo.port ];
  };

  nix.settings.substituters = [
    "https://mirrors.bfsu.edu.cn/nix-channels/store"
    "https://mirror.sjtu.edu.cn/nix-channels/store"
  ];
  environment.systemPackages = with pkgs; [
    temporal-cli
    iproute2
  ];

  boreal = {
    role = "central";
    updater.enable = true;
    vm.adminKeys = [ publicKey ];
    remote.central = {
      listenAddress = "${demo.centralIp}:${toString demo.port}";
      serverCertificate = "${pki}/server.pem";
      serverKey = "${pki}/server.key";
      clientCaCertificate = "${pki}/ca.pem";
      clientCertificatesDirectory = "${pki}/pins";
    };
  };

  containers.compute-01 = {
    path = self.nixosConfigurations.compute-01.config.system.build.toplevel;
    autoStart = true;
    privateNetwork = true;
    # Unlike hostAddress/localAddress on the default veth, an extra veth only
    # installs a route to the peer (no default route in the container).
    extraVeths.${veth} = {
      hostAddress = demo.centralIp;
      localAddress = demo.peripheralIp;
    };
    # NixOS bind mounts are read-only by default. Share only these three files.
    bindMounts = {
      "${demo.clientDirectory}/ca.pem".hostPath = "${pki}/ca.pem";
      "${demo.clientDirectory}/alice-greet.pem".hostPath = "${pki}/client/alice-greet.pem";
      "${demo.clientDirectory}/alice-greet.key".hostPath = "${pki}/client/alice-greet.key";
    };
  };

  systemd.services = {
    "container@compute-01" = {
      requires = [ pkiUnit ];
      after = [ pkiUnit ];
    };

    boreal-broker = {
      requires = [ pkiUnit ];
      wants = [ containerUnit ];
      # The container unit configures the VM's veth address in ExecStartPost.
      # The worker may start before ingress and retry; it doesn't gate boot.
      after = [
        pkiUnit
        containerUnit
      ];
    };

    boreal-demo-pki = {
      description = "Generate disposable Boreal demo mTLS credentials";
      path = [ pkgs.openssl ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        RuntimeDirectory = "boreal-demo-pki";
        RuntimeDirectoryMode = "0700";
        # Keep mounted inodes stable if this unit restarts; /run resets at boot.
        RuntimeDirectoryPreserve = "yes";
        UMask = "0077";
        WorkingDirectory = pkiDirectory;
      };
      script = ''
        set -eu
        if [ -d "${pki}" ]; then
          exit 0
        fi
        temp=$(mktemp -d .pki.XXXXXXXX)
        trap 'rm -rf -- "$PWD/$temp"' EXIT
        (
          cd "$temp"
          mkdir client pins

          openssl req -new -x509 -days 7 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
            -config /dev/null -subj '/CN=Boreal demo CA' -keyout ca.key -out ca.pem \
            -addext 'basicConstraints=critical,CA:TRUE,pathlen:0' \
            -addext 'keyUsage=critical,keyCertSign,cRLSign' \
            -addext 'subjectKeyIdentifier=hash'

          cat > server.ext <<'EOF'
        basicConstraints = critical,CA:FALSE
        keyUsage = critical,digitalSignature
        extendedKeyUsage = serverAuth
        subjectAltName = IP:${demo.centralIp}
        EOF
          openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
            -config /dev/null -subj '/CN=boreal-vm' -keyout server.key -out server.csr
          openssl x509 -req -days 7 -CA ca.pem -CAkey ca.key -set_serial 1 \
            -extfile server.ext -in server.csr -out server.pem

          cat > client.ext <<'EOF'
        basicConstraints = critical,CA:FALSE
        keyUsage = critical,digitalSignature
        extendedKeyUsage = clientAuth
        EOF
          openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
            -config /dev/null -subj '/CN=alice-greet@compute-01' \
            -keyout client/alice-greet.key -out client.csr
          openssl x509 -req -days 7 -CA ca.pem -CAkey ca.key -set_serial 2 \
            -extfile client.ext -in client.csr -out client/alice-greet.pem

          openssl verify -CAfile ca.pem -purpose sslserver -verify_ip ${demo.centralIp} server.pem
          openssl verify -CAfile ca.pem -purpose sslclient client/alice-greet.pem
          cp client/alice-greet.pem pins/alice-greet@compute-01.pem
          rm ca.key server.csr server.ext client.csr client.ext
        )
        mv -T -- "$temp" "${pki}"
      '';
    };
  };
}
