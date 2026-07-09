use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor, SerializeOptions};

/// A gRPC service's callable methods, in the shape the method picker
/// renders directly -- independent of `prost_reflect`'s descriptor types so
/// the UI crate never needs to depend on `prost_reflect` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcMethodInfo {
    pub name: String,
    /// Fully-qualified `package.Service.Method`, used to look the method
    /// back up in the pool when the call is actually made.
    pub full_name: String,
    pub input_type_name: String,
    pub output_type_name: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcServiceInfo {
    pub name: String,
    pub full_name: String,
    pub methods: Vec<GrpcMethodInfo>,
}

/// Lists every service/method in `pool`, in declaration order -- the method
/// picker renders exactly this list, so the caller never touches
/// `prost_reflect` descriptor types directly.
pub fn list_services(pool: &DescriptorPool) -> Vec<GrpcServiceInfo> {
    pool.services()
        .map(|service| GrpcServiceInfo {
            name: service.name().to_string(),
            full_name: service.full_name().to_string(),
            methods: service.methods().map(describe_method).collect(),
        })
        .collect()
}

fn describe_method(method: MethodDescriptor) -> GrpcMethodInfo {
    GrpcMethodInfo {
        name: method.name().to_string(),
        full_name: method.full_name().to_string(),
        input_type_name: method.input().full_name().to_string(),
        output_type_name: method.output().full_name().to_string(),
        client_streaming: method.is_client_streaming(),
        server_streaming: method.is_server_streaming(),
    }
}

/// Builds a `DescriptorPool` from local `.proto` source files via `protox`
/// (pure Rust, no `protoc` binary required) -- the fallback discovery path
/// for servers with reflection disabled. `import_paths` are searched for
/// both `file_paths` themselves and any `import "..."` directives inside
/// them, matching `protoc`'s own `-I` semantics.
pub fn descriptor_pool_from_proto_files(
    file_paths: &[impl AsRef<Path>],
    import_paths: &[impl AsRef<Path>],
) -> Result<DescriptorPool> {
    let mut compiler = protox::Compiler::new(import_paths.iter().map(AsRef::as_ref))
        .context("failed to set up the .proto include paths")?;
    compiler
        .open_files(file_paths.iter().map(AsRef::as_ref))
        .context("failed to compile .proto files")?;
    Ok(compiler.descriptor_pool())
}

/// The reflection-discovery counterpart of
/// [`descriptor_pool_from_proto_files`]: reflection responses arrive as raw
/// `FileDescriptorProto` bytes (the server avoids a `descriptor.proto`
/// dependency by keeping them opaque), so this is the pure, network-free
/// half of that path -- decoding and pool-building only, kept separate so
/// it is unit-testable without a live gRPC server.
pub fn descriptor_pool_from_file_descriptor_proto_bytes(
    file_descriptor_proto_bytes: &[Vec<u8>],
) -> Result<DescriptorPool> {
    use prost::Message;

    let mut seen_names = HashMap::new();
    let mut files = Vec::with_capacity(file_descriptor_proto_bytes.len());
    for bytes in file_descriptor_proto_bytes {
        let file = prost_types::FileDescriptorProto::decode(bytes.as_slice())
            .context("reflection server returned a malformed FileDescriptorProto")?;
        // The reflection service is explicitly allowed to omit files it
        // already sent earlier in the stream, but a single response can
        // still legitimately repeat a shared dependency -- skip duplicates
        // by name rather than letting `DescriptorPool` reject them.
        if let Some(name) = &file.name
            && seen_names.insert(name.clone(), ()).is_some()
        {
            continue;
        }
        files.push(file);
    }

    DescriptorPool::from_file_descriptor_set(prost_types::FileDescriptorSet { file: files })
        .context("reflection server's file descriptors did not form a valid descriptor pool")
}

/// Serializes an empty message skeleton for `message_full_name`, with every
/// field present at its zero value -- "Use Example Message" seeds the
/// editor with this rather than a blank `{}`, so the user sees every field
/// name and type without needing to cross-reference the `.proto` source.
pub fn example_message_json(pool: &DescriptorPool, message_full_name: &str) -> Result<String> {
    let message_descriptor = pool
        .get_message_by_name(message_full_name)
        .with_context(|| {
            format!("message `{message_full_name}` was not found in the descriptor pool")
        })?;
    let message = DynamicMessage::new(message_descriptor);
    let options = SerializeOptions::new().skip_default_fields(false);
    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::pretty(&mut buffer);
    message
        .serialize_with_options(&mut serializer, &options)
        .context("failed to render the example message as JSON")?;
    String::from_utf8(buffer).context("example message JSON was not valid UTF-8")
}

/// Parses `json` into a `DynamicMessage` of the message type named
/// `message_full_name`, for handing to the outgoing gRPC call.
pub fn json_to_dynamic_message(
    pool: &DescriptorPool,
    message_full_name: &str,
    json: &str,
) -> Result<DynamicMessage> {
    let message_descriptor = pool
        .get_message_by_name(message_full_name)
        .with_context(|| {
            format!("message `{message_full_name}` was not found in the descriptor pool")
        })?;
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let message =
        DynamicMessage::deserialize(message_descriptor, &mut deserializer).with_context(|| {
            format!("request JSON does not match the shape of `{message_full_name}`")
        })?;
    deserializer
        .end()
        .context("trailing data after the request JSON")?;
    Ok(message)
}

/// Serializes a `DynamicMessage` response back to JSON for display.
pub fn dynamic_message_to_json(message: &DynamicMessage) -> Result<String> {
    serde_json::to_string_pretty(message).context("failed to render the response message as JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SAMPLE_PROTO: &str = r#"
        syntax = "proto3";
        package greeter;

        message HelloRequest {
            string name = 1;
            int32 count = 2;
        }

        message HelloReply {
            string message = 1;
        }

        service Greeter {
            rpc SayHello (HelloRequest) returns (HelloReply);
            rpc SayHelloStream (HelloRequest) returns (stream HelloReply);
            rpc SayHelloClientStream (stream HelloRequest) returns (HelloReply);
            rpc SayHelloBidi (stream HelloRequest) returns (stream HelloReply);
        }
    "#;

    fn write_sample_proto() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("greeter.proto");
        let mut file = std::fs::File::create(&path).expect("create proto file");
        file.write_all(SAMPLE_PROTO.as_bytes())
            .expect("write proto file");
        (dir, path)
    }

    fn sample_pool() -> DescriptorPool {
        let (_dir, path) = write_sample_proto();
        descriptor_pool_from_proto_files(
            std::slice::from_ref(&path),
            &[path.parent().unwrap().to_path_buf()],
        )
        .unwrap()
    }

    #[test]
    fn compiling_a_local_proto_file_produces_a_usable_descriptor_pool() {
        let pool = sample_pool();
        assert!(pool.get_message_by_name("greeter.HelloRequest").is_some());
        assert!(pool.get_service_by_name("greeter.Greeter").is_some());
    }

    #[test]
    fn list_services_reports_every_method_and_its_streaming_shape() {
        let pool = sample_pool();
        let services = list_services(&pool);
        assert_eq!(services.len(), 1);
        let greeter = &services[0];
        assert_eq!(greeter.full_name, "greeter.Greeter");
        assert_eq!(greeter.methods.len(), 4);

        let unary = greeter
            .methods
            .iter()
            .find(|m| m.name == "SayHello")
            .unwrap();
        assert!(!unary.client_streaming && !unary.server_streaming);
        assert_eq!(unary.input_type_name, "greeter.HelloRequest");
        assert_eq!(unary.output_type_name, "greeter.HelloReply");

        let server_stream = greeter
            .methods
            .iter()
            .find(|m| m.name == "SayHelloStream")
            .unwrap();
        assert!(!server_stream.client_streaming && server_stream.server_streaming);

        let client_stream = greeter
            .methods
            .iter()
            .find(|m| m.name == "SayHelloClientStream")
            .unwrap();
        assert!(client_stream.client_streaming && !client_stream.server_streaming);

        let bidi = greeter
            .methods
            .iter()
            .find(|m| m.name == "SayHelloBidi")
            .unwrap();
        assert!(bidi.client_streaming && bidi.server_streaming);
    }

    #[test]
    fn example_message_json_includes_every_field_at_its_zero_value() {
        let pool = sample_pool();
        let json = example_message_json(&pool, "greeter.HelloRequest").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "");
        assert_eq!(value["count"], 0);
    }

    #[test]
    fn json_round_trips_through_a_dynamic_message() {
        let pool = sample_pool();
        let message =
            json_to_dynamic_message(&pool, "greeter.HelloRequest", r#"{"name":"Ada","count":3}"#)
                .unwrap();
        let json = dynamic_message_to_json(&message).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "Ada");
        assert_eq!(value["count"], 3);
    }

    #[test]
    fn malformed_request_json_is_rejected_with_context_rather_than_panicking() {
        let pool = sample_pool();
        let result = json_to_dynamic_message(&pool, "greeter.HelloRequest", "{not json");
        assert!(result.is_err());
    }

    #[test]
    fn a_json_value_of_the_wrong_shape_is_rejected() {
        let pool = sample_pool();
        // `count` must be a number, not a string -- this must be rejected
        // rather than silently coerced.
        let result = json_to_dynamic_message(
            &pool,
            "greeter.HelloRequest",
            r#"{"name":"Ada","count":"three"}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn descriptor_pool_from_file_descriptor_proto_bytes_deduplicates_repeated_files() {
        let pool = sample_pool();
        let bytes = pool
            .files()
            .map(|file| {
                use prost::Message;
                file.file_descriptor_proto().encode_to_vec()
            })
            .collect::<Vec<_>>();
        // Simulate a reflection server resending the same file twice.
        let mut duplicated = bytes.clone();
        duplicated.extend(bytes);
        let rebuilt = descriptor_pool_from_file_descriptor_proto_bytes(&duplicated).unwrap();
        assert!(
            rebuilt
                .get_message_by_name("greeter.HelloRequest")
                .is_some()
        );
    }

    #[test]
    fn malformed_file_descriptor_proto_bytes_are_rejected_rather_than_panicking() {
        let result = descriptor_pool_from_file_descriptor_proto_bytes(&[
            b"not a file descriptor proto".to_vec(),
        ]);
        assert!(result.is_err());
    }
}
