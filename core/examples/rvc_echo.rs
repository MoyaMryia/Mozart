//! rvc 数据面回显桩（Task 4 smoke 专用）：绑定 `input_socket` 收 InputFrame，
//! 零阶保持 3:1 上采样为 OutputFrame 回发到 pre 绑定的 `output_socket`。
//! 只验证 AI 支路往返，不实现 SPEC-03 控制面与推理；真 rvc 进程规格见
//! SPEC-03，本桩不是其替代规格。

use mozart_core::dgram::FrameSocket;
use mozart_core::{InputFrame, OutputFrame, INPUT_SAMPLES, OUTPUT_SAMPLES};
use std::path::PathBuf;

/// AI 接收缓冲帧数：SPEC-02「接收缓冲限制在 4–8 帧」取上沿。
const QUEUE_FRAMES: usize = 8;

/// 48 kHz 输出 = 16 kHz 输入逐样本复制三份（零阶保持上采样，回听音高不变）。
const _: () = assert!(OUTPUT_SAMPLES == INPUT_SAMPLES * 3);

fn main() {
    let (input_sock, output_sock) = parse_args();
    // pre 先起（绑定 output_socket），echo 后连；次序由 smoke 脚本保证。
    let rx = FrameSocket::<InputFrame>::bind(&input_sock, QUEUE_FRAMES).unwrap_or_else(|err| {
        eprintln!("mozart-rvc-echo: bind {}: {err}", input_sock.display());
        std::process::exit(1);
    });
    let tx = FrameSocket::<OutputFrame>::connect(&output_sock, QUEUE_FRAMES)
        .unwrap_or_else(|err| {
            eprintln!("mozart-rvc-echo: connect {}: {err}", output_sock.display());
            std::process::exit(1);
        });
    eprintln!(
        "mozart-rvc-echo: echo {} -> {}",
        input_sock.display(),
        output_sock.display()
    );
    loop {
        let frame = match rx.recv(None) {
            Ok(frame) => frame,
            Err(err) => {
                eprintln!("mozart-rvc-echo: recv: {err}");
                break;
            }
        };
        // packed 字段按值整块搬出，不取引用。
        let meta = frame.meta;
        let pcm_in = frame.pcm;
        let mut pcm = [0f32; OUTPUT_SAMPLES];
        for (i, slot) in pcm.iter_mut().enumerate() {
            *slot = pcm_in[i / 3];
        }
        if let Err(err) = tx.send(&OutputFrame { meta, pcm }) {
            eprintln!("mozart-rvc-echo: send: {err}");
        }
    }
}

fn parse_args() -> (PathBuf, PathBuf) {
    let mut input = None;
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input-sock" => input = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--output-sock" => {
                output = Some(PathBuf::from(args.next().unwrap_or_else(|| usage())))
            }
            _ => usage(),
        }
    }
    (input.unwrap_or_else(|| usage()), output.unwrap_or_else(|| usage()))
}

fn usage() -> ! {
    eprintln!("mozart-rvc-echo: usage: --input-sock <path> --output-sock <path>");
    std::process::exit(2);
}
