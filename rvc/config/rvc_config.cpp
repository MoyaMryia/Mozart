// rvc_config.cpp — [rvc] 段 + [[models]] 注册表 TOML 解析（toml++）
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

// 特征引擎自动发现：models_dir 下按约定文件名查找，找到即填入。
void discover_engine(std::filesystem::path& target,
                     const std::filesystem::path& models_dir,
                     const char* filename) {
    if (!target.empty() || models_dir.empty()) return;
    auto candidate = models_dir / filename;
    std::error_code ec;
    if (std::filesystem::exists(candidate, ec)) {
        target = std::move(candidate);
    }
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

    // [[models]] 注册表：取 type=rvc 的条目。
    if (const auto* arr = table["models"].as_array()) {
        for (const auto& item : *arr) {
            const auto* entry = item.as_table();
            if (!entry) continue;
            const auto* type_node = (*entry)["type"].as_string();
            if (!type_node || type_node->get() != "rvc") continue;
            const auto* id_node = (*entry)["id"].as_string();
            const auto* path_node = (*entry)["path"].as_string();
            if (!id_node || !path_node) {
                reason = "[[models]] type=rvc entry requires id and path strings";
                return false;
            }
            out.models.push_back({id_node->get(), "rvc", path_node->get()});
        }
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

    // 特征引擎自动发现：显式配置优先，空时从 models_dir 按约定文件名查找。
    discover_engine(out.hubert_path, out.models_dir, "hubert_base_dynamic.onnx");
    discover_engine(out.rmvpe_path, out.models_dir, "rmvpe_dynamic.onnx");
    discover_engine(out.realtime_hubert_path, out.models_dir, "hubert-realtime.onnx");
    discover_engine(out.realtime_rmvpe_path, out.models_dir, "rmvpe-realtime.onnx");

    return true;
}

} // namespace rvc
