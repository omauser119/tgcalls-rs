cmake_minimum_required(VERSION 3.16)
project(tgcalls_tde2e_benchmark LANGUAGES C CXX)

if(NOT IS_DIRECTORY "${TD_SOURCE_DIR}/tde2e")
    message(FATAL_ERROR "Set TD_SOURCE_DIR to the pinned TDLib source checkout")
endif()
set(TD_E2E_ONLY ON CACHE BOOL "")
set(TDUTILS_MIME_TYPE OFF CACHE BOOL "")
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
add_subdirectory("${TD_SOURCE_DIR}" tdlib)

add_library(tgcalls_bench_crypto SHARED provider.cpp)
target_link_libraries(tgcalls_bench_crypto PRIVATE tde2e tdutils)
target_include_directories(tgcalls_bench_crypto PRIVATE "${TD_SOURCE_DIR}" "${TD_SOURCE_DIR}/td/generate/auto")
