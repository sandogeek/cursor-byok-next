//! Process entry for the Cursor API protocol debugger.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a CLI reports its address and errors on the terminal"
)]

use std::path::PathBuf;
use std::process::ExitCode;

use cursor_capture::{Config, DEFAULT_SERVICE_ADDR, Server};

#[tokio::main]
async fn main() -> ExitCode {
    let mut config = Config::default();
    let mut open_browser = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-addr" | "--addr" => {
                if let Some(value) = args.next() {
                    config.service_addr = value;
                }
            }
            "-max-exchanges" | "--max-exchanges" => {
                if let Some(value) = args.next() {
                    config.max_exchanges = value.parse().unwrap_or(config.max_exchanges);
                }
            }
            "-db" | "--db" => {
                if let Some(value) = args.next() {
                    config.database_path = PathBuf::from(value);
                }
            }
            "-open" | "--open" => open_browser = true,
            "-no-open" | "--no-open" => open_browser = false,
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown flag: {other}");
                print_usage();
                return ExitCode::from(2);
            }
        }
    }
    if config.service_addr.is_empty() {
        config.service_addr = DEFAULT_SERVICE_ADDR.into();
    }

    let server = match Server::new(config) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(server.service_addr()).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("启动 API 服务监听失败：{err}");
            return ExitCode::FAILURE;
        }
    };

    println!("Cursor API 调试服务已启动");
    println!("服务地址: http://{}", server.service_addr());
    println!("固定上游: {}", server.upstream_url());
    println!("调试界面: {}", server.ui_url());
    println!("SQLite: {}", server.database_path());
    if open_browser {
        let _ = open::that(server.ui_url());
    }

    let app = server.router();
    if let Err(err) = axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn print_usage() {
    eprintln!(
        "Usage: cursor-capture [--addr 127.0.0.1:9090] [--max-exchanges 200] [--db path] [--no-open]"
    );
}
