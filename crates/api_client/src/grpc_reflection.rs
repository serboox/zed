use anyhow::{Context, Result, bail};
use prost_reflect::DescriptorPool;
use tonic::Request;
use tonic::transport::Channel;
use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
use tonic_reflection::pb::v1::{ServerReflectionRequest, ServerReflectionResponse};

use crate::grpc_descriptor::descriptor_pool_from_file_descriptor_proto_bytes;
use crate::network_runtime::on_network_runtime;

fn reflection_request(message_request: MessageRequest) -> ServerReflectionRequest {
    ServerReflectionRequest {
        host: String::new(),
        message_request: Some(message_request),
    }
}

/// Sends every request in `requests` over a single `ServerReflectionInfo`
/// bidi stream and collects the responses in order -- the reflection
/// service processes stream requests and emits stream responses
/// one-for-one, so submitting the whole batch upfront (rather than an
/// interactive request/wait/request loop) is valid and avoids needing a
/// second channel to feed requests as responses arrive.
async fn call_reflection_info(
    client: &mut ServerReflectionClient<Channel>,
    requests: Vec<ServerReflectionRequest>,
) -> Result<Vec<ServerReflectionResponse>> {
    let expected_count = requests.len();
    let stream = client
        .server_reflection_info(Request::new(tokio_stream::iter(requests)))
        .await
        .context("gRPC reflection request failed")?
        .into_inner();

    let responses: Vec<ServerReflectionResponse> =
        tokio_stream::StreamExt::collect::<Result<Vec<_>, _>>(stream)
            .await
            .context("gRPC reflection stream was interrupted")?;
    if responses.len() != expected_count {
        bail!(
            "reflection server returned {} response(s) for {expected_count} request(s)",
            responses.len()
        );
    }
    Ok(responses)
}

fn extract_file_descriptor_proto_bytes(
    response: ServerReflectionResponse,
    symbol: &str,
) -> Result<Vec<Vec<u8>>> {
    match response.message_response {
        Some(MessageResponse::FileDescriptorResponse(file_response)) => {
            Ok(file_response.file_descriptor_proto)
        }
        Some(MessageResponse::ErrorResponse(error)) => bail!(
            "reflection server rejected `{symbol}`: {}",
            error.error_message
        ),
        other => {
            bail!("reflection server sent an unexpected response type for `{symbol}`: {other:?}")
        }
    }
}

fn extract_service_names(response: ServerReflectionResponse) -> Result<Vec<String>> {
    match response.message_response {
        Some(MessageResponse::ListServicesResponse(list)) => Ok(list
            .service
            .into_iter()
            .map(|service| service.name)
            .collect()),
        Some(MessageResponse::ErrorResponse(error)) => bail!(
            "reflection server rejected list_services: {}",
            error.error_message
        ),
        other => {
            bail!("reflection server sent an unexpected response type for list_services: {other:?}")
        }
    }
}

/// Discovers every service on `address` via gRPC server reflection
/// (`grpc.reflection.v1.ServerReflection`) and builds a `DescriptorPool`
/// from the result -- the zero-setup discovery path for servers that have
/// reflection enabled (the common case). See `grpc_descriptor` for the
/// local-`.proto`-import fallback used when it's disabled.
pub async fn discover_via_reflection(channel: Channel) -> Result<DescriptorPool> {
    on_network_runtime(async move {
        let mut client = ServerReflectionClient::new(channel);

        let list_response = call_reflection_info(
            &mut client,
            vec![reflection_request(MessageRequest::ListServices(
                String::new(),
            ))],
        )
        .await?
        .into_iter()
        .next()
        .context("reflection server sent no response to list_services")?;
        let service_names = extract_service_names(list_response)?;
        if service_names.is_empty() {
            bail!("reflection server reported no services");
        }

        let file_requests: Vec<ServerReflectionRequest> = service_names
            .iter()
            .map(|name| reflection_request(MessageRequest::FileContainingSymbol(name.clone())))
            .collect();
        let file_responses = call_reflection_info(&mut client, file_requests).await?;

        let mut all_file_descriptor_proto_bytes = Vec::new();
        for (service_name, response) in service_names.iter().zip(file_responses) {
            all_file_descriptor_proto_bytes
                .extend(extract_file_descriptor_proto_bytes(response, service_name)?);
        }

        descriptor_pool_from_file_descriptor_proto_bytes(&all_file_descriptor_proto_bytes)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic_reflection::pb::v1::{ErrorResponse, ListServiceResponse, ServiceResponse};

    #[test]
    fn extract_service_names_reads_a_list_services_response() {
        let response = ServerReflectionResponse {
            valid_host: String::new(),
            original_request: None,
            message_response: Some(MessageResponse::ListServicesResponse(ListServiceResponse {
                service: vec![ServiceResponse {
                    name: "greeter.Greeter".to_string(),
                }],
            })),
        };
        assert_eq!(
            extract_service_names(response).unwrap(),
            vec!["greeter.Greeter".to_string()]
        );
    }

    #[test]
    fn extract_service_names_surfaces_a_server_error_response() {
        let response = ServerReflectionResponse {
            valid_host: String::new(),
            original_request: None,
            message_response: Some(MessageResponse::ErrorResponse(ErrorResponse {
                error_code: 12,
                error_message: "not implemented".to_string(),
            })),
        };
        let error = extract_service_names(response).unwrap_err();
        assert!(error.to_string().contains("not implemented"));
    }

    #[test]
    fn extract_file_descriptor_proto_bytes_reads_a_file_descriptor_response() {
        let response = ServerReflectionResponse {
            valid_host: String::new(),
            original_request: None,
            message_response: Some(MessageResponse::FileDescriptorResponse(
                tonic_reflection::pb::v1::FileDescriptorResponse {
                    file_descriptor_proto: vec![b"fake bytes".to_vec()],
                },
            )),
        };
        let bytes = extract_file_descriptor_proto_bytes(response, "greeter.Greeter").unwrap();
        assert_eq!(bytes, vec![b"fake bytes".to_vec()]);
    }

    #[test]
    fn extract_file_descriptor_proto_bytes_surfaces_a_server_error_response() {
        let response = ServerReflectionResponse {
            valid_host: String::new(),
            original_request: None,
            message_response: Some(MessageResponse::ErrorResponse(ErrorResponse {
                error_code: 5,
                error_message: "symbol not found".to_string(),
            })),
        };
        let error = extract_file_descriptor_proto_bytes(response, "greeter.Greeter").unwrap_err();
        assert!(error.to_string().contains("symbol not found"));
    }
}
