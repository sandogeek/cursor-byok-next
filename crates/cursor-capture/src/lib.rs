//! Capture and inspect Cursor API traffic with the original debug UI.

mod capture;
mod config;
mod decode;
mod proxy;
mod store;
mod types;
mod web;

pub use config::{Config, DEBUG_BASE_PATH, DEFAULT_SERVICE_ADDR, DEFAULT_UPSTREAM_URL};
pub use proxy::Server;
pub use types::{ConversationSummary, Exchange, ExchangeSummary, FrameView, Header, Payload};

#[cfg(test)]
mod tests;
