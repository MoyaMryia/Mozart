# 编译配置：告警、消毒器与确定性浮点。
# 这些设置只在此处定义一次，组件不重复声明。

find_program(MOZART_CCACHE ccache)
if(MOZART_CCACHE AND NOT CMAKE_CXX_COMPILER_LAUNCHER)
    set(CMAKE_CXX_COMPILER_LAUNCHER "${MOZART_CCACHE}" CACHE STRING "编译器缓存")
endif()

add_library(mozart_warnings INTERFACE)
target_compile_options(mozart_warnings INTERFACE -Wall -Wextra -Wpedantic)
if(MOZART_WARNINGS_AS_ERRORS)
    target_compile_options(mozart_warnings INTERFACE -Werror)
endif()

if(NOT MOZART_SANITIZE STREQUAL "none")
    message(STATUS "启用消毒器：${MOZART_SANITIZE}")
    add_compile_options(-fsanitize=${MOZART_SANITIZE} -fno-omit-frame-pointer)
    add_link_options(-fsanitize=${MOZART_SANITIZE})
endif()

# 数值敏感的源文件关闭浮点收缩与 fast-math。
# 调用形式：mozart_deterministic_floats(<target> <源文件>...)
function(mozart_deterministic_floats target)
    set_source_files_properties(${ARGN} TARGET_DIRECTORY ${target} PROPERTIES
        COMPILE_OPTIONS "-ffp-contract=off;-fno-fast-math")
endfunction()
