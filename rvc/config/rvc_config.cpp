// rvc_config.cpp — [rvc] 段 TOML 解析（toml++；键名与 Rust 侧 RvcConfig 一致）
#include "config/rvc_config.h"

#include <string>
#include <utility>

#include <toml++/toml.hpp>

namespace rvc {

namespace {

// 读字符串键（std::string / std::filesystem::path 皆可）；缺省沿用当前值。
template <typename StringLike>
bool read_string(const toml::table& table, const char* key,
                 StringLike& out, std::string& reason) {
    const auto* node = table.get(key);
    if (!node) return true;
    if (const auto* value = node->as_string()) {
        out = value->get();
        return true;
    }
    reason = std::string("[rvc].") + key + " must be a string";
    return false;
}

} // namespace

bool load_rvc_config(const std::filesystem::path& toml_path,
                     RvcStartupConfig& out, std::string& reason) {
    std::error_code io_error;
    if (!std::filesystem::exists(toml_path, io_error)) {
        if (io_error) {
            reason = "stat " + toml_path.string() + ": " + io_error.message();
            return false;
        }
        return true;   // 未配置：沿用默认
    }
    toml::table table;
    try {
        table = toml::parse_file(toml_path.native());
    } catch (const toml::parse_error& error) {
        reason = "parse " + toml_path.string() + ": "
            + std::string(error.description());
        return false;
    }
    if (const auto* rvc = table["rvc"].as_table()) {
        if (!read_string(*rvc, "models_dir", out.models_dir, reason)) return false;
        if (!read_string(*rvc, "device", out.device, reason)) return false;
        if (!read_string(*rvc, "hubert_path", out.hubert_path, reason)) return false;
        if (!read_string(*rvc, "rmvpe_path", out.rmvpe_path, reason)) return false;
        if (!read_string(*rvc, "realtime_hubert_path",
                         out.realtime_hubert_path, reason)) return false;
        if (!read_string(*rvc, "realtime_rmvpe_path",
                         out.realtime_rmvpe_path, reason)) return false;
        if (!read_string(*rvc, "input_socket", out.input_socket, reason)) return false;
        if (!read_string(*rvc, "output_socket", out.output_socket, reason)) return false;

        if (const auto* node = rvc->get("half")) {
            if (const auto* value = node->as_boolean()) {
                out.half = value->get();
            } else {
                reason = "[rvc].half must be a boolean";
                return false;
            }
        }
        if (const auto* node = rvc->get("cache_limit")) {
            if (const auto* value = node->as_integer()) {
                const int64_t limit = value->get();
                if (limit < 1 || limit > 64) {
                    reason = "[rvc].cache_limit must be within 1..=64";
                    return false;
                }
                out.cache_limit = static_cast<size_t>(limit);
            } else {
                reason = "[rvc].cache_limit must be an integer";
                return false;
            }
        }
    }
    return true;
}

} // namespace rvc
