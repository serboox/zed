use prost::Message;
use prost_reflect::{DynamicMessage, MessageDescriptor};
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// A `tonic` [`Codec`] for [`DynamicMessage`] -- the schema-less counterpart
/// of `tonic-prost`'s generated-type `ProstCodec`. `DynamicMessage` has no
/// `Default` impl (constructing one needs a [`MessageDescriptor`], which a
/// generic `T::default()` can't provide), so unlike `ProstCodec` this
/// codec carries the response message's descriptor itself and builds each
/// decoded message from it explicitly.
#[derive(Clone)]
pub struct DynamicCodec {
    response_descriptor: MessageDescriptor,
}

impl DynamicCodec {
    pub fn new(response_descriptor: MessageDescriptor) -> Self {
        Self {
            response_descriptor,
        }
    }
}

impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder {
            response_descriptor: self.response_descriptor.clone(),
        }
    }
}

pub struct DynamicEncoder;

impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, buf: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(buf).map_err(|error| {
            Status::internal(format!("failed to encode gRPC request message: {error}"))
        })
    }
}

pub struct DynamicDecoder {
    response_descriptor: MessageDescriptor,
}

impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        let mut message = DynamicMessage::new(self.response_descriptor.clone());
        message.merge(buf).map_err(|error| {
            Status::internal(format!("failed to decode gRPC response message: {error}"))
        })?;
        Ok(Some(message))
    }
}
