//! llama-server 监管桩：不说控制协议（SPEC-05），只保持存活供 daemon 监管测试。
//! 忽略启动参数；真实 llama-server 绑定 --host 的 Unix socket，桩不绑。
//! SIGTERM 走默认处置即退出，落在 daemon 的 2 s 宽限内。

fn main() {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}