# core：帧契约 C 头（单一来源）。Rust 镜像在 core/src/lib.rs。
add_library(mozart_core INTERFACE)
target_include_directories(mozart_core INTERFACE
    "${CMAKE_CURRENT_SOURCE_DIR}/include")
