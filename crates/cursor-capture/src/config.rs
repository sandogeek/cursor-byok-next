//! Configuration and loopback address helpers for the protocol debugger.

use std::net::IpAddr;
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

#[derive(Debug, Clone)]
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
    pub fn normalized(mut self) -> Result<Self, Error> {
        if self.service_addr.trim().is_empty() {
            self.service_addr = DEFAULT_SERVICE_ADDR.into();
        }
        if self.upstream_url.trim().is_empty() {
            self.upstream_url = DEFAULT_UPSTREAM_URL.into();
        }
        if self.max_exchanges <= 0 {
            self.max_exchanges = DEFAULT_MAX_EXCHANGES;
        }
        if self.max_capture_bytes <= 0 {
            self.max_capture_bytes = DEFAULT_MAX_CAPTURE_BYTES;
        }
        if self.max_frames <= 0 {
            self.max_frames = DEFAULT_MAX_FRAMES;
        }
        if self.database_path.as_os_str().is_empty() {
            self.database_path = default_database_path();
        }
        validate_loopback_address(&self.service_addr)?;
        Ok(self)
    }
}

pub fn default_database_path() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".cursor-byok-next").join(DEFAULT_DATABASE_NAME))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATABASE_NAME))
}

pub fn browser_address(address: &str) -> String {
    let Ok((host, port)) = split_host_port(address) else {
        return address.to_string();
    };
    let host = if host.is_empty() || host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    join_host_port(host, port)
}

pub fn validate_loopback_address(address: &str) -> Result<(), Error> {
    let (host, _) = split_host_port(address)
        .map_err(|_| Error::Config(format!("调试服务监听地址无效：{address}")))?;
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(());
    }
    let ip: IpAddr = host
        .parse()
        .map_err(|_| Error::Config("调试服务只能监听本机回环地址".into()))?;
    if ip.is_loopback() {
        Ok(())
    } else {
        Err(Error::Config("调试服务只能监听本机回环地址".into()))
    }
}

fn split_host_port(address: &str) -> Result<(&str, &str), ()> {
    if let Some(rest) = address.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or(())?;
        let port = tail.strip_prefix(':').ok_or(())?;
        if port.is_empty() {
            return Err(());
        }
        return Ok((host, port));
    }
    let (host, port) = address.rsplit_once(':').ok_or(())?;
    if port.is_empty() || port.contains(':') {
        return Err(());
    }
    Ok((host, port))
}

fn join_host_port(host: &str, port: &str) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}
