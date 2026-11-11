mod poller;
mod server;
mod services;
mod state;
mod supervisor;

use mozart_core::config::Config;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let config_path = match parse_args() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("mozart-daemon: {message}");
            return ExitCode::from(2);
        }
    };
    match run(config_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mozart-daemon: {message}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Result<Option<PathBuf>, String> {
    let mut config = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(config)
}

fn run(config_path: Option<PathBuf>) -> Result<(), String> {
    let config = Config::load(config_path.as_deref()).map_err(|err| err.to_string())?;
    server::serve(&config).map_err(|err| format!("server: {err}"))
}
