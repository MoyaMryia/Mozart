//! 协议桩共用逻辑：参数解析、应答组装、连接服务循环。
//! 仅用于测试；不实现音频或推理。

use mozart_core::ctl::{self, Message, Seqpacket};
use serde_json::{json, Value};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;

pub struct StubArgs {
    pub name: &'static str,
    pub socket: PathBuf,
    pub exit_after: Option<Duration>,
    pub reply_delay: Duration,
}

pub fn parse_args(name: &'static str) -> StubArgs {
    let mut socket = None;
    let mut exit_after = None;
    let mut reply_delay = Duration::ZERO;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(args.next().unwrap_or_else(|| usage(name))));
            }
            // daemon spawn 传同一 TOML（--config）；协议桩不读配置，接受即忽略。
            "--config" => {
                let _ = args.next().unwrap_or_else(|| usage(name));
            }
            "--exit-after-ms" => {
                let value = args.next().unwrap_or_else(|| usage(name));
                let millis = value.parse().unwrap_or_else(|_| usage(name));
                exit_after = Some(Duration::from_millis(millis));
            }
            "--delay-ms" => {
                let value = args.next().unwrap_or_else(|| usage(name));
                let millis = value.parse().unwrap_or_else(|_| usage(name));
                reply_delay = Duration::from_millis(millis);
            }
            _ => usage(name),
        }
    }
    StubArgs {
        name,
        socket: socket.unwrap_or_else(|| usage(name)),
        exit_after,
        reply_delay,
    }
}

fn usage(name: &str) -> ! {
    eprintln!(
        "{name}: usage: --socket <path> [--config <path>] [--exit-after-ms <ms>] [--delay-ms <ms>]"
    );
    std::process::exit(2);
}

pub fn ok(data: Value) -> Vec<u8> {
    json!({"status": "ok", "code": ctl::CODE_OK, "reason": "", "data": data})
        .to_string()
        .into_bytes()
}

pub fn fail(code: &str, reason: &str) -> Vec<u8> {
    json!({"status": "error", "code": code, "reason": reason, "data": null})
        .to_string()
        .into_bytes()
}

pub fn body(msg: &Message) -> Result<Value, Vec<u8>> {
    if msg.body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&msg.body)
        .map_err(|err| fail(ctl::CODE_INVALID_ARGUMENT, &err.to_string()))
}

pub fn expect_known(msg: &Message, known: &[u16]) -> Result<(), Vec<u8>> {
    let type_ = msg.header.type_;
    if known.contains(&type_) {
        Ok(())
    } else {
        Err(fail(ctl::CODE_UNKNOWN_COMMAND, "unknown command"))
    }
}

pub fn serve(args: StubArgs, mut handler: impl FnMut(&Message) -> Vec<u8>) -> ! {
    if let Some(delay) = args.exit_after {
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            std::process::exit(0);
        });
    }

    let _ = std::fs::remove_file(&args.socket);
    let listener = Seqpacket::bind(&args.socket).unwrap_or_else(|err| {
        eprintln!("{}: bind {}: {err}", args.name, args.socket.display());
        std::process::exit(1);
    });
    listener.listen(4).unwrap_or_else(|err| {
        eprintln!("{}: listen: {err}", args.name);
        std::process::exit(1);
    });

    loop {
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("{}: accept: {err}", args.name);
                continue;
            }
        };
        loop {
            let msg = match conn.recv(None) {
                Ok(msg) => msg,
                Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
                Err(err) => {
                    eprintln!("{}: recv: {err}", args.name);
                    break;
                }
            };
            let type_ = msg.header.type_;
            let request_id = msg.header.request_id;
            std::thread::sleep(args.reply_delay);
            let reply = handler(&msg);
            if let Err(err) = conn.send(type_, request_id, &reply) {
                eprintln!("{}: send: {err}", args.name);
                break;
            }
        }
    }
}
