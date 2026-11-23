//! Includes generated Cursor protobuf types.
//!
//! prost output is not hand-written, so the style lints that only make sense for
//! authored code are exempted here rather than in the crate root.

pub mod agent {
    #[expect(
        clippy::doc_markdown,
        clippy::large_enum_variant,
        clippy::struct_excessive_bools,
        reason = "prost-generated bindings"
    )]
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/agent.v1.rs"));
    }
}

pub mod inference {
    #[expect(
        clippy::doc_markdown,
        clippy::large_enum_variant,
        clippy::struct_excessive_bools,
        reason = "prost-generated bindings"
    )]
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/aiserver.v1.rs"));
    }
    pub use v1::*;
}

pub mod aiserver {
    pub mod v1 {
        #[derive(Clone, PartialEq, ::prost::Message)]
        pub struct BidiRequestId {
            #[prost(string, tag = "1")]
            pub request_id: String,
        }

        #[derive(Clone, PartialEq, ::prost::Message)]
        pub struct BidiAppendRequest {
            #[prost(string, tag = "1")]
            pub data: String,
            #[prost(message, optional, tag = "2")]
            pub request_id: Option<BidiRequestId>,
            #[prost(int64, tag = "3")]
            pub append_seqno: i64,
            #[prost(bytes = "vec", tag = "4")]
            pub data_binary: Vec<u8>,
        }

        #[derive(Clone, Copy, PartialEq, ::prost::Message)]
        pub struct BidiAppendResponse {}

        #[derive(Clone, Copy, PartialEq, ::prost::Message)]
        pub struct IsConnectedResponse {}
    }
}
