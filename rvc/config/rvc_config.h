// rvc_config.h — [rvc] 段进程启动配置（SPEC-03 参数边界表「进程启动」行）
// ============================================================================
// 部署 TOML 与 Rust 侧（core/src/config.rs::RvcConfig）共用同一 [rvc] 段：
// daemon 校验并使用会话默认值键，本进程校验并使用推理资产键。值的校验
// 在本入口一次完成，内部组件信任已夹取的值。
#ifndef RVC_CONFIG_H
#define RVC_CONFIG_H

#include <cstdint>
#include <filesystem>
#include <string>

namespace rvc {

struct RvcStartupConfig {
    // 模型根目录；空表示未配置（LoadModel 会拒绝而非崩溃）。
    std::filesystem::path models_dir;
    std::string device = "cuda";
    bool half = false;
    size_t cache_limit = 2;
    // 特征引擎与实时资产路径；空表示不配置。
    std::filesystem::path hubert_path;
    std::filesystem::path rmvpe_path;
    std::filesystem::path realtime_hubert_path;
    std::filesystem::path realtime_rmvpe_path;
    // 数据面 socket 名：相对 --socket 所在目录解析（绝对路径原样使用），
    // 默认与 Rust 侧 rvc.input_socket / rvc.output_socket 一致。
    std::string input_socket = "rvc-in.sock";
    std::string output_socket = "rvc-out.sock";
    // 数据面内核缓冲帧数（SPEC-02：接收缓冲限制 4–8 帧，与 pre 侧同量级）。
    size_t queue_frames = 8;
};

// 解析 TOML 的 [rvc] 段；文件不存在时沿用默认值并返回 true
//（部署可不带模型键启动，LoadModel 时再给出原因）。
// 文件存在但读失败/类型不符返回 false 并填 reason。
bool load_rvc_config(const std::filesystem::path& toml_path,
                     RvcStartupConfig& out, std::string& reason);

} // namespace rvc

#endif // RVC_CONFIG_H
