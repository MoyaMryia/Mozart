# rvc：C++ 推理进程。推理核心搬运自 Mozart-Old/rvc-backend（只读归档），
# 数据面换 Unix DGRAM 线包，控制面换 SEQPACKET + ctl.h 信封（SPEC-03）。

find_package(Threads REQUIRED)

# nlohmann_json：模型 config.json 与控制面 JSON 体（find_package 优先，回退 FetchContent）。
find_package(nlohmann_json 3.2.0 QUIET)
if(NOT nlohmann_json_FOUND)
    include(FetchContent)
    FetchContent_Declare(json
        GIT_REPOSITORY https://github.com/nlohmann/json.git
        GIT_TAG v3.11.3
        GIT_SHALLOW TRUE)
    FetchContent_MakeAvailable(json)
endif()

# spdlog：搬运文件日志引用面广（9 个源文件），保留。系统包要求 ≥ 1.14：
# Jetson 系统 1.12 捆绑 fmt 9，GCC 13.3 -O3 触发 array-bounds 假阳性
#（fmt#3354），1.15.1 捆绑 fmt 11 无此问题；不满足版本时 FetchContent。
find_package(spdlog 1.14 QUIET)
if(NOT spdlog_FOUND)
    include(FetchContent)
    FetchContent_Declare(spdlog
        GIT_REPOSITORY https://github.com/gabime/spdlog.git
        GIT_TAG v1.15.1
        GIT_SHALLOW TRUE)
    FetchContent_MakeAvailable(spdlog)
endif()

# toml++：部署配置解析（单头库）；find_package 优先。
find_package(tomlplusplus QUIET)
if(NOT tomlplusplus_FOUND)
    include(FetchContent)
    FetchContent_Declare(tomlplusplus
        GIT_REPOSITORY https://github.com/marzer/tomlplusplus.git
        GIT_TAG v3.4.0
        GIT_SHALLOW TRUE)
    FetchContent_MakeAvailable(tomlplusplus)
endif()

# ---- 推理后端：TRT 优先，找不到回退 ORT；两者都关则引擎为桩 ----
add_library(rvc_backend_flags INTERFACE)
if(MOZART_WITH_TRT)
    find_path(TENSORRT_INCLUDE_DIR NvInfer.h
        PATHS /usr/include/aarch64-linux-gnu /usr/include/x86_64-linux-gnu /usr/include)
    find_path(CUDA_INCLUDE_DIR cuda_runtime_api.h
        PATHS /usr/local/cuda/include /usr/include)
    find_library(TENSORRT_LIBRARY nvinfer
        PATHS /usr/lib/aarch64-linux-gnu /usr/lib/x86_64-linux-gnu /usr/lib)
    find_library(CUDART_LIBRARY cudart
        PATHS /usr/local/cuda/lib64 /usr/lib/aarch64-linux-gnu /usr/lib)
    if(TENSORRT_INCLUDE_DIR AND TENSORRT_LIBRARY AND CUDART_LIBRARY)
        target_include_directories(rvc_backend_flags INTERFACE
            ${TENSORRT_INCLUDE_DIR} ${CUDA_INCLUDE_DIR})
        target_compile_definitions(rvc_backend_flags INTERFACE USE_TENSORRT=1)
        target_link_libraries(rvc_backend_flags INTERFACE ${TENSORRT_LIBRARY} ${CUDART_LIBRARY})
        message(STATUS "rvc: TensorRT enabled (${TENSORRT_LIBRARY})")
    else()
        message(STATUS "rvc: TensorRT not found — engines fall back to ONNX Runtime")
    endif()
endif()

if(MOZART_WITH_ORT)
    # 两个部署布局都探测：JetPack 包在 /usr/lib/aarch64-linux-gnu +
    # /usr/include/onnxruntime；手动安装（本机 Jetson）在 /usr/local。
    # 显式 cache 变量优先，用于其余布局。
    set(ONNX_RUNTIME_PATH "" CACHE PATH
        "ONNX Runtime 库路径；默认按 JetPack 与 /usr/local 探测")
    set(ONNX_RUNTIME_INCLUDE_DIR "" CACHE PATH
        "ONNX Runtime 头文件路径；默认按 JetPack 与 /usr/local 探测")
    find_library(ONNX_RUNTIME_LIB onnxruntime
        HINTS "${ONNX_RUNTIME_PATH}" /usr/lib/aarch64-linux-gnu /usr/local/lib)
    find_path(RVC_ORT_INCLUDE onnxruntime_cxx_api.h
        HINTS "${ONNX_RUNTIME_INCLUDE_DIR}" /usr/include/onnxruntime /usr/local/include)
    if(ONNX_RUNTIME_LIB AND RVC_ORT_INCLUDE)
        message(STATUS "rvc: ONNX Runtime enabled (${ONNX_RUNTIME_LIB})")
    else()
        message(FATAL_ERROR "MOZART_WITH_ORT=ON 需要找到 ONNX Runtime 头文件与库；"
            "主机无 GPU 测试请用 OFF，非常规位置用 ONNX_RUNTIME_PATH / "
            "ONNX_RUNTIME_INCLUDE_DIR 指定。")
    endif()
endif()

# ---- 推理核心运行库（搬运自旧 rvc-backend 的最小集）+ 数据面 ----
add_library(mozart_rvc_runtime STATIC
    src/rvc/audio_worker.cpp
    src/rvc/file_worker.cpp
    src/rvc/streaming_pipeline.cpp
    src/rvc/trt_engine.cpp
    src/rvc/pipeline.cpp
    src/rvc/inferencer.cpp
    src/rvc/feature_extractor.cpp
    src/rvc/model_loader.cpp
    src/rvc/onnx_engine.cpp
    src/rvc/index_search.cpp
    transport/unix_dgram_stream.cpp
    control/ctl_link.cpp
    control/stream_session.cpp
    config/rvc_config.cpp
)
# include/：搬运核心的旧布局（rvc/*.hpp）；仓库内新模块按目录相对包含
# （transport/、control/、config/），因此把组件根也加入搜索路径。
target_include_directories(mozart_rvc_runtime PUBLIC
    "${CMAKE_CURRENT_SOURCE_DIR}/include"
    "${CMAKE_CURRENT_SOURCE_DIR}")
target_link_libraries(mozart_rvc_runtime
    PUBLIC mozart_core nlohmann_json::nlohmann_json spdlog::spdlog
           tomlplusplus::tomlplusplus Threads::Threads
    PRIVATE rvc_backend_flags mozart_warnings)
if(ONNX_RUNTIME_LIB)
    target_compile_definitions(mozart_rvc_runtime PUBLIC USE_ONNX)
    target_include_directories(mozart_rvc_runtime PUBLIC ${RVC_ORT_INCLUDE})
    target_link_libraries(mozart_rvc_runtime PUBLIC ${ONNX_RUNTIME_LIB})
endif()

# 入口：--socket/--config → [rvc] 配置 → StreamSession 服务循环。
add_executable(mozart-rvc main.cpp)
target_link_libraries(mozart-rvc PRIVATE mozart_rvc_runtime mozart_warnings)
