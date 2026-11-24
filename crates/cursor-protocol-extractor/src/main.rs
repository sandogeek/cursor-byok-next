//! CLI for extracting agent.v1 / aiserver.v1 protobuf schemas from Cursor JS bundles.

#![expect(
    clippy::print_stderr,
    reason = "a CLI reports its progress and errors on the terminal"
)]

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use cursor_protocol_extractor::{Strictness, extract_from_files};

fn main() -> ExitCode {
    let mut inputs = Vec::new();
    let mut output = None;
    let mut strict = true;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-input" | "--input" => {
                let Some(path) = args.next() else {
                    return missing_value("-input");
                };
                inputs.push(PathBuf::from(path));
            }
            "-output" | "--output" => {
                let Some(path) = args.next() else {
                    return missing_value("-output");
                };
                output = Some(PathBuf::from(path));
            }
            "-strict" | "--strict" => strict = true,
            "-no-strict" | "--no-strict" => strict = false,
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other if !other.starts_with('-') => inputs.push(PathBuf::from(other)),
            other => {
                eprintln!("unknown flag: {other}");
                print_usage();
                return ExitCode::from(2);
            }
        }
    }
    if inputs.is_empty() {
        print_usage();
        return ExitCode::from(2);
    }
    let Some(output) = output else {
        eprintln!("-output is required");
        return ExitCode::from(2);
    };
    let strictness = if strict {
        Strictness::Strict
    } else {
        Strictness::Lenient
    };
    match extract_from_files(&inputs, &output, strictness) {
        Ok(result) => {
            eprint!("{}", result.diagnostics);
            for warning in &result.write_warnings {
                eprintln!("warning: {warning}");
            }
            let messages = result.messages.len();
            let enums = result.enums.len();
            let services = result.services.len();
            eprintln!("提取完成: {messages} 个消息, {enums} 个枚举, {services} 个服务");
            eprintln!("Output directory: {}", output.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn missing_value(flag: &str) -> ExitCode {
    eprintln!("{flag} needs a value");
    print_usage();
    ExitCode::from(2)
}

fn print_usage() {
    eprintln!(
        "Usage: cursor-protocol-extractor -input <js-bundle> [-input <js-bundle> ...] -output <dir> [--no-strict]"
    );
}
