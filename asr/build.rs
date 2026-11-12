//! sherpa-onnx 链接开关：设置 SHERPA_ONNX_LIB_DIR 时链真实 C 库并置 has_sherpa；
//! 未设置时不链接，推理走 stub（本机 cargo check/test 无需 sherpa 库）。

fn main() {
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");
    println!("cargo::rustc-check-cfg=cfg(has_sherpa)");
    if let Ok(dir) = std::env::var("SHERPA_ONNX_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-lib=dylib=sherpa-onnx-c-api");
        println!("cargo:rustc-cfg=has_sherpa");
    }
}