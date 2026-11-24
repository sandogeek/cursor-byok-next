//! Process entry for the Cursor API protocol debugger.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a CLI reports its address and errors on the terminal"
)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use cursor_capture::{Config, Server};

#[tokio::main]
async fn main() -> ExitCode {
    let mut config = Config::default();
    let mut open_browser = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-addr" | "--addr" => match args.next() {
                Some(value) => config.service_addr = value,
                None => return report_usage_error("--addr needs a value"),
            },
            "-max-exchanges" | "--max-exchanges" => {
                let Some(value) = args.next() else {
                    return report_usage_error("--max-exchanges needs a value");
                };
                match value.parse() {
                    Ok(max) => config.max_exchanges = max,
                    Err(err) => {
                        return report_usage_error(&format!(
                            "--max-exchanges must be a positive integer: {err}"
                        ));
                    }
                }
            }
            "-db" | "--db" => match args.next() {
                Some(value) => config.database_path = PathBuf::from(value),
                None => return report_usage_error("--db needs a path"),
            },
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
    let server = match Server::new(config) {
        Ok(server) => Arc::new(server),
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(server.service_addr()).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("无法监听 {}：{err}", server.service_addr());
            return ExitCode::FAILURE;
        }
    };

    println!("Cursor API 调试服务已启动");
    println!("服务地址: http://{}", server.service_addr());
    println!("固定上游: {}", server.upstream_url());
    println!("调试界面: {}", server.ui_url());
    println!("SQLite: {}", server.database_path());
    if open_browser {
        report_launch_failure(&server);
    }

    let app = Arc::clone(&server).router();
    if let Err(err) = axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Opens the debug UI, reporting a failure instead of silently doing nothing.
fn report_launch_failure(server: &Server) {
    let url = server.ui_url();
    if let Err(err) = open::that(&url) {
        eprintln!("无法自动打开浏览器（{url}）：{err}");
    }
}

fn report_usage_error(message: &str) -> ExitCode {
    eprintln!("{message}");
    print_usage();
    ExitCode::from(2)
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        eprintln!("无法监听中断信号，只能通过关闭进程退出：{err}");
    }
}

fn print_usage() {
    eprintln!(
        "Usage: cursor-capture [--addr 127.0.0.1:9090] [--max-exchanges 200] [--db path] [--no-open]"
    );
}
