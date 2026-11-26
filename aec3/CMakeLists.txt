find_package(PkgConfig QUIET)
if(NOT PkgConfig_FOUND)
    message(STATUS "PkgConfig not found; skipping mozart_aec3")
    return()
endif()

set(_mozart_old_pkg_config_path "$ENV{PKG_CONFIG_PATH}")
set(_mozart_extra_pc_paths "")
if(DEFINED ENV{MOZART_ASSETS_DIR} AND NOT "$ENV{MOZART_ASSETS_DIR}" STREQUAL "")
    list(APPEND _mozart_extra_pc_paths
        "$ENV{MOZART_ASSETS_DIR}/lib/pkgconfig"
        "$ENV{MOZART_ASSETS_DIR}/lib/aarch64-linux-gnu/pkgconfig")
endif()
if(DEFINED ENV{HOME} AND NOT "$ENV{HOME}" STREQUAL "")
    list(APPEND _mozart_extra_pc_paths
        "$ENV{HOME}/mozart-assets/lib/pkgconfig"
        "$ENV{HOME}/mozart-assets/lib/aarch64-linux-gnu/pkgconfig")
endif()
if(_mozart_extra_pc_paths)
    list(JOIN _mozart_extra_pc_paths ":" _mozart_extra_pc_str)
    if(_mozart_old_pkg_config_path)
        set(ENV{PKG_CONFIG_PATH} "${_mozart_extra_pc_str}:${_mozart_old_pkg_config_path}")
    else()
        set(ENV{PKG_CONFIG_PATH} "${_mozart_extra_pc_str}")
    endif()
endif()

pkg_check_modules(WEBRTC_AP QUIET IMPORTED_TARGET webrtc-audio-processing-1)
set(ENV{PKG_CONFIG_PATH} "${_mozart_old_pkg_config_path}")

if(NOT WEBRTC_AP_FOUND)
    message(STATUS "webrtc-audio-processing-1 not found; skipping mozart_aec3")
    return()
endif()

add_library(mozart_aec3 SHARED src/mozart_aec3.cpp)
target_include_directories(mozart_aec3 PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/include")
target_link_libraries(mozart_aec3 PRIVATE PkgConfig::WEBRTC_AP mozart_warnings)
set(_mozart_aec3_rpath "$ORIGIN")
if(WEBRTC_AP_LIBRARY_DIRS)
    list(APPEND _mozart_aec3_rpath ${WEBRTC_AP_LIBRARY_DIRS})
endif()
set_target_properties(mozart_aec3 PROPERTIES
    BUILD_RPATH "${_mozart_aec3_rpath}"
    INSTALL_RPATH "$ORIGIN"
)

if(BUILD_TESTING)
    add_executable(t_aec3_wrapper ../tests/test_aec3_wrapper.cpp)
    target_link_libraries(t_aec3_wrapper PRIVATE mozart_aec3 mozart_warnings)
    add_test(NAME aec3_wrapper COMMAND t_aec3_wrapper)
    set_tests_properties(aec3_wrapper PROPERTIES LABELS "unit" TIMEOUT 30)
endif()
