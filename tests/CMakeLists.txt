# 构建契约用例：证明工具链、标准、告警与 CTest 接线符合约定。
add_executable(t_build_contract test_build_contract.cpp)
target_link_libraries(t_build_contract PRIVATE mozart_warnings)
add_test(NAME build_contract COMMAND t_build_contract)
set_tests_properties(build_contract PROPERTIES LABELS "unit")

# 控制信封契约：C 头与 Rust 镜像的尺寸、字节序一致。
add_executable(t_control_contract test_control_contract.cpp)
target_link_libraries(t_control_contract PRIVATE mozart_core mozart_warnings)
add_test(NAME control_contract COMMAND t_control_contract)
set_tests_properties(control_contract PROPERTIES LABELS "unit")

# rvc 数据面线包与 Unix DGRAM 流语义（无 GPU 依赖）。
add_executable(t_rvc_transport test_rvc_transport.cpp)
target_link_libraries(t_rvc_transport PRIVATE mozart_rvc_runtime mozart_warnings)
add_test(NAME rvc_transport COMMAND t_rvc_transport)
set_tests_properties(rvc_transport PROPERTIES LABELS "unit" TIMEOUT 30)

# 帧字节共享夹具目录：C++ 与 Rust（core/tests/frame_fixture.rs）读同一组文件。
set(MOZART_FRAME_FIXTURE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/fixtures/frame")
target_compile_definitions(t_control_contract PRIVATE
    MOZART_FRAME_FIXTURE_DIR="${MOZART_FRAME_FIXTURE_DIR}")
target_compile_definitions(t_rvc_transport PRIVATE
    MOZART_FRAME_FIXTURE_DIR="${MOZART_FRAME_FIXTURE_DIR}")

# rvc 泵接线：AudioWorker × 新数据面 × StreamingRvc（确定性假管线，无 GPU 依赖）。
add_executable(t_rvc_pump test_rvc_pump.cpp)
target_link_libraries(t_rvc_pump PRIVATE mozart_rvc_runtime mozart_warnings)
add_test(NAME rvc_pump COMMAND t_rvc_pump)
set_tests_properties(rvc_pump PROPERTIES LABELS "unit" TIMEOUT 60)

# rvc 文件整段批处理：FileWorker × 阻塞数据面（mock 管线，无 GPU 依赖）。
add_executable(t_rvc_file test_rvc_file.cpp)
target_link_libraries(t_rvc_file PRIVATE mozart_rvc_runtime mozart_warnings)
add_test(NAME rvc_file COMMAND t_rvc_file)
set_tests_properties(rvc_file PROPERTIES LABELS "unit" TIMEOUT 60)

# StreamSession 六命令控制面语义（桩后端，无 GPU 依赖）。
add_executable(t_rvc_control test_rvc_control.cpp)
target_link_libraries(t_rvc_control PRIVATE mozart_rvc_runtime mozart_warnings)
add_test(NAME rvc_control COMMAND t_rvc_control)
set_tests_properties(rvc_control PROPERTIES LABELS "unit" TIMEOUT 30)

# StreamingRvc 块调度/连续性/静音/断续单测（搬运自旧 tests/test_streaming.cpp）。
add_executable(t_rvc_streaming test_rvc_streaming.cpp)
target_link_libraries(t_rvc_streaming PRIVATE mozart_rvc_runtime mozart_warnings)
add_test(NAME rvc_streaming COMMAND t_rvc_streaming)
set_tests_properties(rvc_streaming PROPERTIES LABELS "unit" TIMEOUT 60)

# 重采样抗镜像/抗混叠：合成信号频谱度量（无 GPU 依赖）。
add_executable(t_resample test_resample.cpp)
target_link_libraries(t_resample PRIVATE mozart_rvc_runtime mozart_warnings)
add_test(NAME resample COMMAND t_resample)
set_tests_properties(resample PROPERTIES LABELS "unit" TIMEOUT 30)

# 文件分窗几何：HuBERT 帧数反推窗输入（纯算术，逐层卷积独立 oracle，无 GPU 依赖）。
add_executable(t_rvc_window_geometry test_rvc_window_geometry.cpp)
target_link_libraries(t_rvc_window_geometry PRIVATE mozart_rvc_runtime mozart_warnings)
add_test(NAME rvc_window_geometry COMMAND t_rvc_window_geometry)
set_tests_properties(rvc_window_geometry PROPERTIES LABELS "unit" TIMEOUT 30)
