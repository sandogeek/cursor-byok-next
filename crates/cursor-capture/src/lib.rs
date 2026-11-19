//! Capture and inspect Cursor API traffic with the original debug UI.

mod capture;
mod config;
mod decode;
mod proxy;
mod store;
mod types;
mod web;

pub use config::{Config, DEFAULT_SERVICE_ADDR, DEFAULT_UPSTREAM_URL, DEBUG_BASE_PATH};
pub use proxy::Server;
pub use types::{ConversationSummary, Exchange, ExchangeSummary, FrameView, Header, Payload};

#[cfg(test)]
mod tests;
