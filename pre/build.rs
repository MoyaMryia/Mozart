//! 可选 libmozart_aec3 动态库链接（复用 asr/build.rs 模式）：
//! 设置 MOZART_AEC3_LIB_DIR 时链接真实 AEC3 包装库并置 has_aec3；
//! 未设置时不链接，AEC 保持未链接桩状态，本机 cargo test/check 无需外部 C++ 库。

fn main() {
    println!("cargo:rerun-if-env-changed=MOZART_AEC3_LIB_DIR");
    println!("cargo::rustc-check-cfg=cfg(has_aec3)");
    if let Ok(dir) = std::env::var("MOZART_AEC3_LIB_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            println!("cargo:rustc-link-search=native={dir}");
            println!("cargo:rustc-link-lib=dylib=mozart_aec3");
            println!("cargo:rustc-cfg=has_aec3");
        }
    }
}
