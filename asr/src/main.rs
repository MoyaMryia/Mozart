//! mozart-asr：识别服务进程（SPEC-05）。
//! 当前为契约空壳：解析 --socket/--config、校验配置后退出；
//! 收包/解码/复核/输出/控制线程在后续任务实现。

use clap::Parser;
use mozart_core::config::Config;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "mozart-asr", version, about = "Mozart 识别服务")]
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
    // 入口校验一次：[asr] 等配置段的合法性在 Config::load 内完成。
    if let Err(err) = Config::load(cli.config.as_deref()) {
        eprintln!("mozart-asr: {err}");
        return ExitCode::FAILURE;
    }
    eprintln!("mozart-asr: skeleton only, socket {}", cli.socket.display());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_contract_matches_supervisor() {
        let cli = Cli::try_parse_from(["mozart-asr", "--socket", "/run/mozart/asr.sock"]).unwrap();
        assert_eq!(cli.socket, PathBuf::from("/run/mozart/asr.sock"));
        assert!(cli.config.is_none());

        let cli = Cli::try_parse_from([
            "mozart-asr",
            "--socket",
            "asr.sock",
            "--config",
            "mozart.toml",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("mozart.toml")));

        assert!(Cli::try_parse_from(["mozart-asr"]).is_err());
        assert!(Cli::try_parse_from(["mozart-asr", "--socket"]).is_err());
        assert!(Cli::try_parse_from(["mozart-asr", "--socket", "s", "--bogus"]).is_err());
    }
}