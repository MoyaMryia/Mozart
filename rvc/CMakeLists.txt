# rvc：C++ 推理进程。当前为占位，后续搬入旧 rvc-backend 并包 StreamSession。
add_executable(mozart-rvc main.cpp)
target_link_libraries(mozart-rvc PRIVATE mozart_core)
