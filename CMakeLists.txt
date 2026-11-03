cmake_minimum_required(VERSION 3.22)
project(Mozart VERSION 0.1.0 LANGUAGES CXX)

include(CTest)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

option(MOZART_WARNINGS_AS_ERRORS "把编译器警告当作错误" ON)
option(MOZART_WITH_ORT "链接 ONNX Runtime" OFF)
option(MOZART_WITH_TRT "链接 TensorRT" OFF)
set(MOZART_SANITIZE "none" CACHE STRING "消毒器：none、address、thread 或 undefined")

# 选项定义之后再加载编译配置，避免模块读到未定义的缓存变量。
list(APPEND CMAKE_MODULE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/cmake")
include(MozartCompiler)

# 组件按目录存在与否自动纳入构建。骨架阶段只有 tests。
set(MOZART_COMPONENTS core pre rvc daemon)
foreach(component IN LISTS MOZART_COMPONENTS)
    if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/${component}/CMakeLists.txt")
        add_subdirectory(${component})
    endif()
endforeach()

if(BUILD_TESTING)
    add_subdirectory(tests)
endif()
