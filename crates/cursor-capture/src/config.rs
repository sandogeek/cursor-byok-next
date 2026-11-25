//! Configuration and loopback address helpers for the protocol debugger.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::types::Error;

pub const DEFAULT_SERVICE_ADDR: &str = "127.0.0.1:9090";
pub const DEFAULT_UPSTREAM_URL: &str = "https://api2.cursor.sh";
pub const DEBUG_BASE_PATH: &str = "/__debuger__";
pub const DEFAULT_MAX_EXCHANGES: i32 = 200;
pub const DEFAULT_MAX_CAPTURE_BYTES: i32 = 2 << 20;
pub const DEFAULT_MAX_FRAMES: i32 = 2000;
pub const DEFAULT_DATABASE_NAME: &str = "cursor-proxy-debugger.db";
pub const MAX_CONNECT_FRAME_BYTES: usize = 64 << 20;

/// Everything the debugger needs to start: where it listens, what it captures,
/// and where it stores the captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub service_addr: String,
    pub upstream_url: String,
    pub max_exchanges: i32,
    pub max_capture_bytes: i32,
    pub max_frames: i32,
    pub database_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            service_addr: DEFAULT_SERVICE_ADDR.into(),
            upstream_url: DEFAULT_UPSTREAM_URL.into(),
            max_exchanges: DEFAULT_MAX_EXCHANGES,
            max_capture_bytes: DEFAULT_MAX_CAPTURE_BYTES,
            max_frames: DEFAULT_MAX_FRAMES,
            database_path: default_database_path(),
        }
    }
}

impl Config {
    /// Replaces blank fields with defaults and validates the listen address.
    ///
    /// A limit that is zero or negative falls back to its default rather than
    /// being rejected, so a caller can pass through an unset CLI flag.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when `service_addr` is not a loopback address
    /// with an explicit port.
    pub fn normalized(mut self) -> Result<Self, Error> {
        if self.service_addr.trim().is_empty() {
            self.service_addr = DEFAULT_SERVICE_ADDR.into();
        }
        if self.upstream_url.trim().is_empty() {
            self.upstream_url = DEFAULT_UPSTREAM_URL.into();
        }
        self.max_exchanges = positive_or(self.max_exchanges, DEFAULT_MAX_EXCHANGES);
        self.max_capture_bytes = positive_or(self.max_capture_bytes, DEFAULT_MAX_CAPTURE_BYTES);
        self.max_frames = positive_or(self.max_frames, DEFAULT_MAX_FRAMES);
        if self.database_path.as_os_str().is_empty() {
            self.database_path = default_database_path();
        }
        validate_loopback_address(&self.service_addr)?;
        Ok(self)
    }
}

fn positive_or(value: i32, fallback: i32) -> i32 {
    if value > 0 { value } else { fallback }
}

/// Where the capture database lives when the caller does not choose.
#[must_use]
pub fn default_database_path() -> PathBuf {
    dirs::home_dir().map_or_else(
        || PathBuf::from(DEFAULT_DATABASE_NAME),
        |home| home.join(".cursor-byok-next").join(DEFAULT_DATABASE_NAME),
    )
}

/// The address a human should open, with wildcard hosts mapped to loopback.
#[must_use]
pub fn browser_address(address: &str) -> String {
    let Ok(addr) = LoopbackAddress::parse(address) else {
        return address.to_owned();
    };
    addr.browser()
}

/// A validated `host:port` that the debugger may listen on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackAddress {
    /// `127.0.0.1`, `::1`, or a literal `localhost`.
    host: String,
    port: u16,
}

impl LoopbackAddress {
    /// Parses `host:port`, accepting only loopback hosts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the address has no port, the port is not
    /// a number, or the host is not loopback.
    pub fn parse(address: &str) -> Result<Self, Error> {
        if let Ok(socket) = address.parse::<SocketAddr>() {
            if !socket.ip().is_loopback() {
                return Err(Error::Config(format!(
                    "调试服务只能监听本机回环地址：{address}"
                )));
            }
            return Ok(Self {
                host: socket.ip().to_string(),
                port: socket.port(),
            });
        }
        match address.rsplit_once(':') {
            Some((host, port)) if host.eq_ignore_ascii_case("localhost") => {
                let port = port
                    .parse()
                    .map_err(|_| Error::Config(format!("调试服务监听端口无效：{address}")))?;
                Ok(Self {
                    host: "127.0.0.1".to_owned(),
                    port,
                })
            }
            _ => Err(Error::Config(format!("调试服务监听地址无效：{address}"))),
        }
    }

    /// The address a browser on this machine can reach.
    #[must_use]
    pub fn browser(&self) -> String {
        let host = self.host.as_str();
        if host.contains(':') {
            format!("[{host}]:{}", self.port)
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// Rejects listen addresses that other machines could reach.
///
/// # Errors
///
/// Returns [`Error::Config`] for a malformed address or a non-loopback host.
pub fn validate_loopback_address(address: &str) -> Result<(), Error> {
    LoopbackAddress::parse(address).map(|_| ())
}
