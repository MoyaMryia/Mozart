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
