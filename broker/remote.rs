use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use boreal_common::catalog::Catalog;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{NoServerSessionStorage, ParsedCertificate, WebPkiClientVerifier};
use rustls::{
    DigitallySignedStruct, DistinguishedName, RootCertStore, ServerConfig, SignatureScheme,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, UnixTime, pem::PemObject};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, server::TlsStream};

use crate::proxy::WorkerBinding;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    certificate: PathBuf,
    private_key: PathBuf,
    client_ca: PathBuf,
    replicas: Vec<Replica>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replica {
    owner: String,
    worker: String,
    certificate: PathBuf,
}

type Bindings = BTreeMap<Vec<u8>, Arc<WorkerBinding>>;

#[derive(Debug)]
struct PinnedVerifier {
    ca: Arc<dyn ClientCertVerifier>,
    bindings: Arc<Bindings>,
}

impl ClientCertVerifier for PinnedVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.ca.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        leaf: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self.ca.verify_client_cert(leaf, intermediates, now)?;
        if !self.bindings.contains_key(leaf.as_ref()) {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.ca.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.ca.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.ca.supported_verify_schemes()
    }
}

#[derive(Clone)]
pub struct RemoteIngress {
    acceptor: TlsAcceptor,
    bindings: Arc<Bindings>,
}

impl RemoteIngress {
    pub fn bind(
        path: &Path,
        catalog: &Catalog,
        expected_uid: u32,
    ) -> Result<(TcpListener, Self), String> {
        let cfg: Config = serde_json::from_reader(checked_file(path, expected_uid, false)?)
            // Do not include any config contents in startup errors.
            .map_err(|e| {
                format!(
                    "invalid remote worker config at line {}, column {}",
                    e.line(),
                    e.column()
                )
            })?;
        let ingress = Self::from_config(&cfg, catalog, expected_uid)?;
        let listener = TcpListener::bind(cfg.listen)
            .and_then(|listener| {
                listener.set_nonblocking(true)?;
                Ok(listener)
            })
            .map_err(|e| format!("bind remote worker listener {}: {e}", cfg.listen))?;
        Ok((listener, ingress))
    }

    fn from_config(cfg: &Config, catalog: &Catalog, expected_uid: u32) -> Result<Self, String> {
        if cfg.replicas.is_empty() {
            return Err("remote worker config has no replicas".into());
        }
        let mut bindings = BTreeMap::new();
        for replica in &cfg.replicas {
            catalog
                .worker_and_package(&replica.owner, &replica.worker)
                .map_err(|_| {
                    format!(
                        "remote worker binding {}/{} is not in the catalog",
                        replica.owner, replica.worker
                    )
                })?;
            let mut certs = certificates(&replica.certificate, expected_uid)?;
            if certs.len() != 1 {
                return Err("remote worker pin must contain exactly one leaf certificate".into());
            }
            let leaf = certs.remove(0);
            ParsedCertificate::try_from(&leaf)
                .map_err(|_| "invalid remote worker pin certificate")?;
            if bindings
                .insert(
                    leaf.to_vec(),
                    Arc::new(WorkerBinding {
                        owner: replica.owner.clone(),
                        task_queue: format!("{}-{}", replica.owner, replica.worker),
                    }),
                )
                .is_some()
            {
                return Err("duplicate remote worker leaf certificate pin".into());
            }
        }
        let bindings = Arc::new(bindings);

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = RootCertStore::empty();
        for cert in certificates(&cfg.client_ca, expected_uid)? {
            roots
                .add(cert)
                .map_err(|_| "invalid remote worker client CA certificate")?;
        }
        let ca =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                .build()
                .map_err(|_| "invalid remote worker client CA configuration")?;

        let key_file = checked_file(&cfg.private_key, expected_uid, true)?;
        let mut keys = PrivateKeyDer::pem_reader_iter(key_file);
        let key = keys
            .next()
            .and_then(Result::ok)
            .ok_or("invalid remote worker TLS private key PEM")?;
        if keys.next().is_some() {
            return Err("remote worker TLS key file must contain one private key".into());
        }
        let mut tls = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| "invalid remote worker TLS protocol configuration")?
            .with_client_cert_verifier(Arc::new(PinnedVerifier {
                ca,
                bindings: Arc::clone(&bindings),
            }))
            .with_single_cert(certificates(&cfg.certificate, expected_uid)?, key)
            .map_err(|_| "invalid remote worker TLS server certificate or key")?;
        tls.alpn_protocols = vec![b"h2".to_vec()];
        // Authenticate the peer again on each connection. In particular do not
        // let a session ticket outlive the client certificate's validity.
        tls.session_storage = Arc::new(NoServerSessionStorage {});
        tls.send_tls13_tickets = 0;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(tls)),
            bindings,
        })
    }

    pub async fn accept(
        &self,
        stream: TcpStream,
    ) -> Option<(TlsStream<TcpStream>, Arc<WorkerBinding>)> {
        let tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.acceptor.accept(stream))
            .await
            .ok()?
            .ok()?;
        let session = tls.get_ref().1;
        if session.alpn_protocol() != Some(b"h2") {
            return None;
        }
        let leaf = session.peer_certificates()?.first()?;
        let binding = Arc::clone(self.bindings.get(leaf.as_ref())?);
        Some((tls, binding))
    }
}

fn checked_file(path: &Path, expected_uid: u32, private: bool) -> Result<impl Read, String> {
    if !path.is_absolute() {
        return Err("remote worker config and credential paths must be absolute".into());
    }
    let check = |meta: &std::fs::Metadata| {
        let forbidden = if private { 0o077 } else { 0o022 };
        if !meta.is_file() || meta.uid() != expected_uid || meta.mode() & forbidden != 0 {
            return Err(format!(
                "unsafe remote worker file ownership, mode or type: {}",
                path.display()
            ));
        }
        if meta.size() > MAX_FILE_BYTES {
            return Err(format!("remote worker file too large: {}", path.display()));
        }
        Ok(())
    };
    let io_err = |e| format!("open remote worker file {}: {e}", path.display());
    // Follow deployment-managed symlinks (e.g. /etc -> Nix store), but check
    // the opened file too so we never consume a user-owned replacement.
    check(&std::fs::metadata(path).map_err(io_err)?)?;
    let file = File::open(path).map_err(io_err)?;
    check(&file.metadata().map_err(io_err)?)?;
    Ok(BufReader::new(file.take(MAX_FILE_BYTES)))
}

fn certificates(path: &Path, expected_uid: u32) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs = CertificateDer::pem_reader_iter(checked_file(path, expected_uid, false)?)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("invalid remote worker certificate PEM: {}", path.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "missing remote worker certificate: {}",
            path.display()
        ));
    }
    Ok(certs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::ClientConfig;
    use rustls_pki_types::ServerName;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use tokio_rustls::TlsConnector;

    #[test]
    fn remote_config_requires_absolute_nonwritable_owned_files_and_known_workers() {
        let dir = tempfile::tempdir().unwrap();
        let uid = boreal_common::os::current_euid();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(checked_file(Path::new("relative.json"), uid, false).is_err());
        assert!(checked_file(&path, uid.wrapping_add(1), false).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(checked_file(&path, uid, false).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(checked_file(&path, uid, false).is_ok());
        assert!(checked_file(&path, uid, true).is_err());

        let catalog = Catalog::from_yaml("", &Default::default()).unwrap();
        let cfg = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            certificate: path.clone(),
            private_key: path.clone(),
            client_ca: path.clone(),
            replicas: vec![Replica {
                owner: "alice".into(),
                worker: "greet".into(),
                certificate: path,
            }],
        };
        assert!(
            RemoteIngress::from_config(&cfg, &catalog, uid)
                .err()
                .unwrap()
                .contains("not in the catalog")
        );
    }

    fn openssl(dir: &Path, args: &[&str]) {
        let result = Command::new("openssl")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "ephemeral OpenSSL fixture generation failed"
        );
    }

    fn ca(dir: &Path, name: &str) {
        openssl(
            dir,
            &[
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:prime256v1",
                "-nodes",
                "-days",
                "1",
                "-subj",
                &format!("/CN={name}"),
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign",
                "-out",
                &format!("{name}.pem"),
                "-keyout",
                &format!("{name}.key"),
            ],
        );
    }

    fn leaf(dir: &Path, name: &str, ca: &str, serial: &str, subject: &str, usage: &str) {
        openssl(
            dir,
            &[
                "req",
                "-new",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:prime256v1",
                "-nodes",
                "-subj",
                subject,
                "-out",
                &format!("{name}.csr"),
                "-keyout",
                &format!("{name}.key"),
            ],
        );
        std::fs::write(dir.join("leaf.ext"), format!(
            "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage={usage}\nsubjectAltName=DNS:localhost\n"
        )).unwrap();
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-days",
                "1",
                "-set_serial",
                serial,
                "-extfile",
                "leaf.ext",
                "-CA",
                &format!("{ca}.pem"),
                "-CAkey",
                &format!("{ca}.key"),
                "-in",
                &format!("{name}.csr"),
                "-out",
                &format!("{name}.pem"),
            ],
        );
    }

    async fn handshake(
        ingress: &RemoteIngress,
        listener: &tokio::net::TcpListener,
        dir: &Path,
        identity: Option<&str>,
        alpn: Option<&[u8]>,
    ) -> Option<Arc<WorkerBinding>> {
        let mut roots = RootCertStore::empty();
        let uid = boreal_common::os::current_euid();
        for cert in certificates(&dir.join("ca.pem"), uid).unwrap() {
            roots.add(cert).unwrap();
        }
        let client =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots);
        let mut client = if let Some(name) = identity {
            let certs = certificates(&dir.join(format!("{name}.pem")), uid).unwrap();
            let key = PrivateKeyDer::from_pem_file(dir.join(format!("{name}.key"))).unwrap();
            client.with_client_auth_cert(certs, key).unwrap()
        } else {
            client.with_no_client_auth()
        };
        client.alpn_protocols = alpn.map(|p| vec![p.to_vec()]).unwrap_or_default();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let connector = TlsConnector::from(Arc::new(client));
        let (server, client) = tokio::join!(
            ingress.accept(server),
            connector.connect(ServerName::try_from("localhost").unwrap(), stream),
        );
        if server.is_some() {
            assert_eq!(
                client.unwrap().get_ref().1.alpn_protocol(),
                Some(b"h2".as_slice())
            );
        }
        server.map(|(_, binding)| binding)
    }

    #[tokio::test]
    #[ignore = "requires openssl on PATH to generate ephemeral certificate fixtures"]
    async fn remote_tls_requires_chain_pin_and_h2_and_rejects_duplicate_pins() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        ca(p, "ca");
        ca(p, "foreign");
        leaf(p, "server", "ca", "1", "/CN=localhost", "serverAuth");
        leaf(p, "replica", "ca", "2", "/CN=replica", "clientAuth");
        // A distinct certificate with the same subject and the same valid CA.
        leaf(p, "unlisted", "ca", "3", "/CN=replica", "clientAuth");
        leaf(
            p,
            "foreign-replica",
            "foreign",
            "4",
            "/CN=replica",
            "clientAuth",
        );
        let catalog = Catalog::from_yaml(
            "greet-package:\n  workflow: GreetWorkflow\n  description: Test\n  args: []\n",
            &[("alice".into(), "user: alice\npermissions:\n  read: false\n  write: false\nworkers:\n  greet:\n    package: greet-package\n    permissions:\n      read: false\n      write: false\n".into())].into(),
        ).unwrap();
        let uid = boreal_common::os::current_euid();
        let cfg = p.join("remote.json");
        let replica = |name| serde_json::json!({"owner": "alice", "worker": "greet", "certificate": p.join(name)});
        let write_config = |pins| {
            let config = serde_json::json!({
                "listen": "127.0.0.1:0", "certificate": p.join("server.pem"),
                "privateKey": p.join("server.key"), "clientCa": p.join("ca.pem"), "replicas": pins,
            });
            std::fs::write(&cfg, config.to_string()).unwrap();
        };
        write_config(vec![replica("replica.pem"), replica("replica.pem")]);
        assert!(
            RemoteIngress::bind(&cfg, &catalog, uid)
                .err()
                .unwrap()
                .contains("duplicate")
        );
        write_config(vec![replica("replica.pem"), replica("foreign-replica.pem")]);
        let (listener, ingress) = RemoteIngress::bind(&cfg, &catalog, uid).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let auth = handshake(&ingress, &listener, p, Some("replica"), Some(b"h2"))
            .await
            .unwrap();
        assert_eq!((&*auth.owner, &*auth.task_queue), ("alice", "alice-greet"));
        for identity in [Some("unlisted"), Some("foreign-replica"), None] {
            assert!(
                handshake(&ingress, &listener, p, identity, Some(b"h2"))
                    .await
                    .is_none()
            );
        }
        for alpn in [Some(b"http/1.1".as_slice()), None] {
            assert!(
                handshake(&ingress, &listener, p, Some("replica"), alpn)
                    .await
                    .is_none()
            );
        }
    }
}
