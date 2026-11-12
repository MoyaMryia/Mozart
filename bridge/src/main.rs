//! mozart-bridge：字幕桥进程（SPEC-05）。
//! 当前为契约空壳：解析 --socket/--config、校验配置后退出；
//! 控制/事件/字幕/翻译/播报线程在后续任务实现。

use clap::Parser;
use mozart_core::config::Config;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "mozart-bridge", version, about = "Mozart 字幕桥")]
struct Cli {
    /// 控制面 socket 路径（daemon 传入）
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    /// 配置文件；缺省按 MOZART_CONFIG、./mozart.toml 顺序查找
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // 入口校验一次：[bridge] 等配置段的合法性在 Config::load 内完成。
    if let Err(err) = Config::load(cli.config.as_deref()) {
        eprintln!("mozart-bridge: {err}");
        return ExitCode::FAILURE;
    }
    eprintln!("mozart-bridge: skeleton only, socket {}", cli.socket.display());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_contract_matches_supervisor() {
        let cli =
            Cli::try_parse_from(["mozart-bridge", "--socket", "/run/mozart/bridge.sock"]).unwrap();
        assert_eq!(cli.socket, PathBuf::from("/run/mozart/bridge.sock"));
        assert!(cli.config.is_none());

        let cli = Cli::try_parse_from([
            "mozart-bridge",
            "--socket",
            "bridge.sock",
            "--config",
            "mozart.toml",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("mozart.toml")));

        assert!(Cli::try_parse_from(["mozart-bridge"]).is_err());
        assert!(Cli::try_parse_from(["mozart-bridge", "--socket"]).is_err());
        assert!(Cli::try_parse_from(["mozart-bridge", "--socket", "s", "--bogus"]).is_err());
    }
}