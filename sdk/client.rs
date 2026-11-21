//! Connect to Temporal using the deployment's client settings.

use temporalio_client::{
    Client, ClientOptions, Connection, GrpcCompression, envconfig::LoadClientConfigProfileOptions,
};

use crate::Error;

/// Connects using Temporal's standard address, namespace, and TLS settings.
pub async fn connect() -> Result<Client, Error> {
    connect_loaded(None).await
}

/// Connects to another namespace using the same address and TLS settings.
///
/// Call from an activity, not from workflow code. The broker authorizes the
/// authenticated worker, not the namespace supplied here.
pub async fn connect_namespace(namespace: &str) -> Result<Client, Error> {
    if !valid_namespace(namespace) {
        return Err("invalid Temporal namespace".into());
    }
    connect_loaded(Some(namespace.to_owned())).await
}

pub(crate) fn load_config() -> Result<(temporalio_client::ConnectionOptions, ClientOptions), Error>
{
    Ok(ClientOptions::load_from_config(
        LoadClientConfigProfileOptions::default(),
    )?)
}

fn valid_namespace(namespace: &str) -> bool {
    let bytes = namespace.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 255
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

async fn connect_loaded(namespace: Option<String>) -> Result<Client, Error> {
    let (mut connection_options, mut client_options) = load_config()?;
    connection_options.grpc_compression = GrpcCompression::None;
    if let Some(namespace) = namespace {
        client_options.namespace = namespace;
    }
    let connection = Connection::connect(connection_options).await?;
    Ok(Client::new(connection, client_options)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_namespace_validation() {
        for invalid in ["", " ", "alice/prep", "-alice"] {
            assert!(!valid_namespace(invalid));
        }
        assert!(valid_namespace("alice"));
        assert!(valid_namespace("shared"));
    }
}
