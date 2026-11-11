// main.cpp — mozart-rvc 推理进程入口（SPEC-03）
// ============================================================================
// 启动参数：--socket <控制面路径> [--config <TOML>]（与 mozart-pre 同一约定，
// daemon 经 supervisor 传递）。配置解析顺序：显式 --config > MOZART_CONFIG >
// ./mozart.toml > 默认值；只读 [rvc] 段（进程启动键见 SPEC-03 参数边界表）。
// 退出：SIGTERM/SIGINT 置停止标志，serve 循环 100 ms 内返回并停流
//（第一版无 UnloadModel/Shutdown 命令，SPEC-03）。
#include <atomic>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <memory>
#include <optional>
#include <string>

#include <spdlog/spdlog.h>

#include "config/rvc_config.h"
#include "control/stream_session.h"

namespace {

// 停止标志：信号处理器只做原子存储（与 mozart-pre 同一纪律）。
std::atomic<bool> g_stop{false};

void on_signal(int) { g_stop.store(true, std::memory_order_relaxed); }

struct Args {
    std::filesystem::path socket;
    std::optional<std::filesystem::path> config;
};

int usage() {
    std::fprintf(stderr, "usage: mozart-rvc --socket <path> [--config <path>]\n");
    return 2;
}

std::optional<Args> parse_args(int argc, char** argv) {
    Args args;
    bool have_socket = false;
    for (int i = 1; i < argc; ++i) {
        const std::string flag = argv[i];
        if (flag == "--socket" && i + 1 < argc) {
            args.socket = argv[++i];
            have_socket = true;
        } else if (flag == "--config" && i + 1 < argc) {
            args.config = argv[++i];
        } else {
            return std::nullopt;
        }
    }
    if (!have_socket) return std::nullopt;
    return args;
}

// 配置解析顺序与 core/src/config.rs::Config::resolve_path 一致。
std::optional<std::filesystem::path> resolve_config(
    const std::optional<std::filesystem::path>& explicit_path) {
    if (explicit_path) return explicit_path;
    if (const char* env = std::getenv("MOZART_CONFIG")) {
        if (*env != '\0') return std::filesystem::path(env);
    }
    const std::filesystem::path local = "mozart.toml";
    std::error_code io_error;
    if (std::filesystem::exists(local, io_error) && !io_error) return local;
    return std::nullopt;
}

// 数据面 socket 名相对控制 socket 所在目录解析（绝对名原样使用）；
// 控制与数据 socket 同属 run_dir，daemon 传入的即完整控制路径。
std::filesystem::path resolve_data_socket(
    const std::filesystem::path& control_socket, const std::string& name) {
    const std::filesystem::path as_path(name);
    if (as_path.is_absolute()) return as_path;
    return control_socket.parent_path() / as_path;
}

} // namespace

int main(int argc, char** argv) {
    const auto args = parse_args(argc, argv);
    if (!args) return usage();

    rvc::RvcStartupConfig startup;
    {
        std::string reason;
        const auto config_path = resolve_config(args->config);
        if (!rvc::load_rvc_config(config_path.value_or(std::filesystem::path{}),
                                  startup, reason)) {
            std::fprintf(stderr, "mozart-rvc: %s\n", reason.c_str());
            return 1;
        }
        spdlog::info("mozart-rvc config: {} (models_dir={})",
                     config_path ? config_path->string() : "<defaults>",
                     startup.models_dir.empty() ? "<unset>"
                                                : startup.models_dir.string());
    }

    std::signal(SIGTERM, on_signal);
    std::signal(SIGINT, on_signal);

    rvc::InferenceBackendConfig backend_config;
    backend_config.models_dir = startup.models_dir;
    backend_config.device = startup.device;
    backend_config.half = startup.half;
    backend_config.cache_limit = startup.cache_limit;
    backend_config.hubert_path = startup.hubert_path;
    backend_config.rmvpe_path = startup.rmvpe_path;
    backend_config.realtime_hubert_path = startup.realtime_hubert_path;
    backend_config.realtime_rmvpe_path = startup.realtime_rmvpe_path;
    backend_config.input_socket =
        resolve_data_socket(args->socket, startup.input_socket);
    backend_config.output_socket =
        resolve_data_socket(args->socket, startup.output_socket);
    backend_config.queue_frames = startup.queue_frames;

    try {
        rvc::CtlListener listener(args->socket);
        auto backend = std::make_unique<rvc::InferenceBackend>(backend_config);
        rvc::StreamSession session(std::move(backend));
        session.serve(listener, g_stop);
    } catch (const std::exception& error) {
        std::fprintf(stderr, "mozart-rvc: %s\n", error.what());
        return 1;
    }
    return 0;
}
