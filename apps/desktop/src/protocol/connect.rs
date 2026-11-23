//! Encodes and decodes Connect protocol frames.
//!
//! A Connect message on the wire is `[flags u8][length u32 BE][payload]`; the
//! end-of-stream marker is a frame whose flags carry [`END_STREAM_FLAG`].

use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;

use crate::{Error, Result};

/// Set on the frame that ends a server stream.
pub const END_STREAM_FLAG: u8 = 0x02;
/// Set on a frame whose payload is compressed.
const COMPRESSED_FLAG: u8 = 0x01;
/// Header bytes preceding every payload.
const ENVELOPE_BYTES: usize = 5;
/// Only the compressed bit and the end-stream bit are defined.
const KNOWN_FLAGS: u8 = COMPRESSED_FLAG | END_STREAM_FLAG;
/// The payload of the frame that ends a stream: an empty JSON-protobuf message.
const END_STREAM_PAYLOAD: &[u8] = b"{}";

/// One Connect frame: the flag byte plus the payload it announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The raw flag byte; [`Frame::is_end_stream`] answers the common question.
    pub flags: u8,
    pub payload: Bytes,
}

impl Frame {
    /// Whether this frame ends the stream.
    #[must_use]
    pub fn is_end_stream(&self) -> bool {
        self.flags & END_STREAM_FLAG != 0
    }

    /// Whether the payload is compressed, so the caller must inflate it.
    #[must_use]
    pub fn is_compressed(&self) -> bool {
        self.flags & COMPRESSED_FLAG != 0
    }
}

/// Reads the payload length out of a Connect envelope's header. Callers have
/// already checked that the header is present.
fn envelope_payload_len(body: &[u8]) -> Result<usize> {
    let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
    usize::try_from(length)
        .map_err(|_| Error::Protocol("Connect frame is longer than the address space".into()))
}

/// Wraps one protobuf message in a Connect envelope.
///
/// # Errors
///
/// Returns [`Error::Encode`] when the message itself fails to encode, and
/// [`Error::Protocol`] when it would exceed the 4 GiB the length prefix holds.
pub fn encode_message<M: Message>(message: &M) -> Result<Bytes> {
    let len = message.encoded_len();
    let len_u32 =
        u32::try_from(len).map_err(|_| Error::Protocol("Connect payload exceeds 4 GiB".into()))?;
    let mut output = BytesMut::with_capacity(ENVELOPE_BYTES + len);
    output.put_u8(0);
    output.put_u32(len_u32);
    message.encode(&mut output)?;
    Ok(output.freeze())
}

/// The empty `{}` frame that ends a server stream.
#[must_use]
pub fn encode_end_stream() -> Bytes {
    let payload = END_STREAM_PAYLOAD;
    let mut output = BytesMut::with_capacity(ENVELOPE_BYTES + payload.len());
    output.put_u8(END_STREAM_FLAG);
    output.put_u32(u32::try_from(payload.len()).expect("payload length is a constant two bytes"));
    output.extend_from_slice(payload);
    output.freeze()
}

/// Decodes a unary body, tolerating both enveloped and bare protobuf.
///
/// # Errors
///
/// Returns [`Error::Decode`] when neither shape parses as `M`.
pub fn decode_unary<M: Message + Default>(body: &[u8]) -> Result<M> {
    // `>=`: an empty message is a five-byte envelope with a zero length.
    if body.len() >= ENVELOPE_BYTES {
        let length = envelope_payload_len(body)?;
        if body[0] & KNOWN_FLAGS == 0 && length == body.len() - ENVELOPE_BYTES {
            return Ok(M::decode(&body[ENVELOPE_BYTES..])?);
        }
    }
    Ok(M::decode(body)?)
}

/// Whether a buffered unary body carries the 5-byte Connect envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    Enveloped,
    Bare,
}

impl Framing {
    #[must_use]
    pub fn is_enveloped(self) -> bool {
        self == Self::Enveloped
    }
}

/// Splits an optional Connect unary envelope off `body`.
///
/// # Errors
///
/// Returns [`Error::Protocol`] when the body is a framed message with flags
/// that cannot be rewritten, such as a compressed or terminal frame.
pub fn split_unary<'a>(body: &'a [u8], label: &str) -> Result<(Framing, &'a [u8])> {
    if body.len() < ENVELOPE_BYTES {
        return Ok((Framing::Bare, body));
    }
    let flags = body[0];
    let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
    if usize::try_from(length).ok() != Some(body.len() - ENVELOPE_BYTES) {
        return Ok((Framing::Bare, body));
    }
    if flags != 0 {
        return Err(Error::Protocol(format!(
            "cannot patch compressed or terminal {label} frame: flags={flags}"
        )));
    }
    Ok((Framing::Enveloped, &body[ENVELOPE_BYTES..]))
}

/// Wraps an encoded unary payload in a Connect envelope when `framing` asks for it.
///
/// # Errors
///
/// Returns [`Error::Protocol`] when an enveloped payload exceeds the 4 GiB the
/// length prefix holds.
pub fn wrap_unary(payload: &[u8], framing: Framing) -> Result<Bytes> {
    if !framing.is_enveloped() {
        return Ok(Bytes::copy_from_slice(payload));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| Error::Protocol("Connect payload exceeds 4 GiB".into()))?;
    let mut output = BytesMut::with_capacity(ENVELOPE_BYTES + payload.len());
    output.put_u8(0);
    output.put_u32(length);
    output.extend_from_slice(payload);
    Ok(output.freeze())
}

/// Splits a streamed body into frames.
///
/// # Errors
///
/// Returns [`Error::Protocol`] when the body ends mid-envelope or mid-payload.
pub fn decode_frames(mut body: &[u8]) -> Result<Vec<Frame>> {
    let mut frames = Vec::new();
    while !body.is_empty() {
        if body.len() < ENVELOPE_BYTES {
            return Err(Error::Protocol("truncated Connect envelope".into()));
        }
        let flags = body[0];
        let length = envelope_payload_len(body)?;
        body = &body[ENVELOPE_BYTES..];
        if body.len() < length {
            return Err(Error::Protocol("truncated Connect payload".into()));
        }
        frames.push(Frame {
            flags,
            payload: Bytes::copy_from_slice(&body[..length]),
        });
        body = &body[length..];
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encoded_message_round_trips_through_a_single_frame() {
        let message = crate::protocol::proto::aiserver::v1::IsConnectedResponse {};
        let encoded = encode_message(&message).unwrap();
        let frames = decode_frames(&encoded).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].is_end_stream());
        assert!(
            decode_unary::<crate::protocol::proto::aiserver::v1::IsConnectedResponse>(&encoded)
                .is_ok()
        );
    }

    #[test]
    fn the_end_stream_marker_is_a_frame_with_no_message() {
        let end = encode_end_stream();
        let frames = decode_frames(&end).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_end_stream());
        assert_eq!(frames[0].payload.as_ref(), b"{}");
    }

    #[test]
    fn a_body_that_stops_mid_envelope_is_rejected() {
        assert!(decode_frames(&[0x00, 0x00]).is_err());
        assert!(decode_frames(&[0x00, 0x00, 0x00, 0x00, 0x05, 0x01]).is_err());
    }
}
