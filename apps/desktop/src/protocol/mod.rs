//! Cursor wire protocol primitives used by the local loop.

mod connect;
mod json;
pub mod proto;

pub use connect::{
    chunk_ends_stream, decode_unary, encode_end_stream, encode_message, rewrite_unary,
};
pub(crate) use json::{prost_json, prost_struct};
