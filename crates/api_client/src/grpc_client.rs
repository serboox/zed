use anyhow::{Context, Result, bail};
use prost_reflect::DescriptorPool;
use tonic::Request;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::grpc_descriptor::GrpcMethodInfo;
use crate::grpc_dynamic_codec::DynamicCodec;
use crate::network_runtime::on_network_runtime;

/// TLS/mTLS settings for a gRPC connection. All fields are plaintext file
/// paths, not secret material itself -- unlike `AuthConfig`'s password/token
/// fields, there is nothing here for `api_client_ui`'s store to redact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrpcTlsConfig {
    pub enabled: bool,
    /// Overrides the platform's trust store with a specific CA certificate
    /// -- typically needed for a self-signed dev server.
    pub ca_certificate_path: Option<String>,
    /// Client certificate for mTLS. Both this and `client_key_path` must be
    /// set together or not at all.
    pub client_certificate_path: Option<String>,
    pub client_key_path: Option<String>,
    /// Overrides the TLS SNI/verification hostname when it differs from
    /// the address's own host (e.g. connecting by IP).
    pub domain_name: Option<String>,
}

fn build_tls_config(tls: &GrpcTlsConfig) -> Result<ClientTlsConfig> {
    let mut config = ClientTlsConfig::new();
    if let Some(path) = &tls.ca_certificate_path {
        let pem = std::fs::read(path)
            .with_context(|| format!("failed to read CA certificate at {path}"))?;
        config = config.ca_certificate(Certificate::from_pem(pem));
    }
    match (&tls.client_certificate_path, &tls.client_key_path) {
        (Some(cert_path), Some(key_path)) => {
            let cert = std::fs::read(cert_path)
                .with_context(|| format!("failed to read client certificate at {cert_path}"))?;
            let key = std::fs::read(key_path)
                .with_context(|| format!("failed to read client key at {key_path}"))?;
            config = config.identity(Identity::from_pem(cert, key));
        }
        (None, None) => {}
        _ => {
            bail!("mTLS needs both a client certificate and a client key -- only one was provided")
        }
    }
    if let Some(domain_name) = &tls.domain_name {
        config = config.domain_name(domain_name.clone());
    }
    Ok(config)
}

/// Connects to `address` (e.g. `https://localhost:50051` or
/// `http://localhost:50051` for plaintext), applying TLS/mTLS when
/// `tls.enabled`. Runs on the shared network Tokio runtime -- see
/// `network_runtime` for why that's necessary.
pub async fn connect_channel(address: String, tls: GrpcTlsConfig) -> Result<Channel> {
    on_network_runtime(async move {
        let mut endpoint = Endpoint::from_shared(address.clone())
            .with_context(|| format!("`{address}` is not a valid gRPC endpoint URI"))?;
        if tls.enabled {
            endpoint = endpoint
                .tls_config(build_tls_config(&tls)?)
                .context("failed to build TLS configuration")?;
        }
        endpoint
            .connect()
            .await
            .with_context(|| format!("failed to connect to {address}"))
    })
    .await
}

fn apply_metadata(
    request: &mut Request<prost_reflect::DynamicMessage>,
    metadata: &[(String, String)],
) -> Result<()> {
    for (key, value) in metadata {
        let metadata_key = tonic::metadata::MetadataKey::from_bytes(key.as_bytes())
            .with_context(|| format!("`{key}` is not a valid gRPC metadata key"))?;
        let metadata_value = tonic::metadata::MetadataValue::try_from(value.as_str())
            .with_context(|| format!("`{value}` is not a valid gRPC metadata value"))?;
        request.metadata_mut().append(metadata_key, metadata_value);
    }
    Ok(())
}

/// Performs a single unary gRPC call using `prost_reflect::DynamicMessage`
/// end to end -- no generated client code needed, since `DynamicCodec`
/// encodes/decodes purely from the descriptor pool's schema.
pub async fn call_unary(
    channel: Channel,
    pool: DescriptorPool,
    method: GrpcMethodInfo,
    request_json: String,
    metadata: Vec<(String, String)>,
) -> Result<String> {
    on_network_runtime(async move {
        let request_message = crate::grpc_descriptor::json_to_dynamic_message(
            &pool,
            &method.input_type_name,
            &request_json,
        )?;
        let output_descriptor = pool
            .get_message_by_name(&method.output_type_name)
            .with_context(|| {
                format!(
                    "output message `{}` was not found in the descriptor pool",
                    method.output_type_name
                )
            })?;

        let mut request = Request::new(request_message);
        apply_metadata(&mut request, &metadata)?;

        let path = grpc_method_path(&method.full_name)?;
        let mut client = tonic::client::Grpc::new(channel);
        client
            .ready()
            .await
            .context("gRPC transport is not ready")?;
        let response = client
            .unary(request, path, DynamicCodec::new(output_descriptor))
            .await
            .map_err(|status| anyhow::anyhow!("gRPC call failed: {status}"))?;

        crate::grpc_descriptor::dynamic_message_to_json(response.get_ref())
    })
    .await
}

/// Performs a server-streaming gRPC call, connecting fresh and pushing
/// each response message (as pretty JSON) into the returned channel as it
/// arrives -- returns immediately once the call is accepted, rather than
/// waiting for the stream to finish, so the caller can render responses as
/// they come in. `Err(_)` items are call/decode failures partway through
/// the stream; the channel closes once the server ends the stream or an
/// error occurs.
pub fn call_server_streaming(
    address: String,
    tls: GrpcTlsConfig,
    pool: DescriptorPool,
    method: GrpcMethodInfo,
    request_json: String,
    metadata: Vec<(String, String)>,
) -> Result<async_channel::Receiver<Result<String, String>>> {
    let (sender, receiver) = async_channel::unbounded();

    crate::network_runtime::spawn_detached_on_network_runtime(async move {
        let result = run_server_streaming_call(
            address,
            tls,
            pool,
            method,
            request_json,
            metadata,
            sender.clone(),
        )
        .await;
        if let Err(error) = result {
            let _ = sender.send(Err(error.to_string())).await;
        }
    })?;

    Ok(receiver)
}

async fn run_server_streaming_call(
    address: String,
    tls: GrpcTlsConfig,
    pool: DescriptorPool,
    method: GrpcMethodInfo,
    request_json: String,
    metadata: Vec<(String, String)>,
    sender: async_channel::Sender<Result<String, String>>,
) -> Result<()> {
    use futures::StreamExt;

    let channel = connect_channel(address, tls).await?;
    let request_message = crate::grpc_descriptor::json_to_dynamic_message(
        &pool,
        &method.input_type_name,
        &request_json,
    )?;
    let output_descriptor = pool
        .get_message_by_name(&method.output_type_name)
        .with_context(|| {
            format!(
                "output message `{}` was not found in the descriptor pool",
                method.output_type_name
            )
        })?;

    let mut request = Request::new(request_message);
    apply_metadata(&mut request, &metadata)?;
    let path = grpc_method_path(&method.full_name)?;

    let mut client = tonic::client::Grpc::new(channel);
    client
        .ready()
        .await
        .context("gRPC transport is not ready")?;
    let mut stream = client
        .server_streaming(request, path, DynamicCodec::new(output_descriptor))
        .await
        .map_err(|status| anyhow::anyhow!("gRPC call failed: {status}"))?
        .into_inner();

    while let Some(item) = stream.next().await {
        let json_result = item
            .map_err(|status| anyhow::anyhow!("gRPC stream error: {status}"))
            .and_then(|message| crate::grpc_descriptor::dynamic_message_to_json(&message));
        let channel_item = json_result.map_err(|error| error.to_string());
        if sender.send(channel_item).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Builds the `/package.Service/Method` HTTP/2 path tonic's transport
/// expects, from a method's fully-qualified `package.Service.Method` name
/// (the descriptor pool's own naming convention).
fn grpc_method_path(method_full_name: &str) -> Result<tonic::codegen::http::uri::PathAndQuery> {
    let Some((service_full_name, method_name)) = method_full_name.rsplit_once('.') else {
        bail!("`{method_full_name}` is not a fully-qualified gRPC method name");
    };
    format!("/{service_full_name}/{method_name}")
        .parse()
        .with_context(|| format!("`{method_full_name}` does not form a valid gRPC call path"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_method_path_builds_the_expected_http2_path() {
        let path = grpc_method_path("greeter.Greeter.SayHello").unwrap();
        assert_eq!(path.as_str(), "/greeter.Greeter/SayHello");
    }

    #[test]
    fn a_method_name_without_a_package_separator_is_rejected() {
        assert!(grpc_method_path("SayHello").is_err());
    }

    #[test]
    fn mismatched_mtls_cert_and_key_paths_are_rejected() {
        let tls = GrpcTlsConfig {
            enabled: true,
            client_certificate_path: Some("/tmp/cert.pem".to_string()),
            client_key_path: None,
            ..Default::default()
        };
        assert!(build_tls_config(&tls).is_err());
    }
}
