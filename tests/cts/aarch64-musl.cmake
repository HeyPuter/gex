set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)

if(NOT DEFINED ENV{GEX_CTS_TOOLCHAIN})
    message(FATAL_ERROR "GEX_CTS_TOOLCHAIN must name the aarch64-linux-musl toolchain root")
endif()
set(GEX_CTS_TOOLCHAIN "$ENV{GEX_CTS_TOOLCHAIN}")
set(CMAKE_C_COMPILER "${GEX_CTS_TOOLCHAIN}/bin/aarch64-linux-musl-gcc")
set(CMAKE_CXX_COMPILER "${GEX_CTS_TOOLCHAIN}/bin/aarch64-linux-musl-g++")
set(CMAKE_AR "${GEX_CTS_TOOLCHAIN}/bin/aarch64-linux-musl-ar")
set(CMAKE_RANLIB "${GEX_CTS_TOOLCHAIN}/bin/aarch64-linux-musl-ranlib")
set(CMAKE_STRIP "${GEX_CTS_TOOLCHAIN}/bin/aarch64-linux-musl-strip")

# Keep the guest closure to the normal musl loader plus the project's EGL/GL
# libraries. Otherwise a browser run also has to package a toolchain-specific
# libstdc++ and libgcc_s beside the official CTS executable.
set(CMAKE_EXE_LINKER_FLAGS_INIT "-static-libstdc++ -static-libgcc")

set(CMAKE_FIND_ROOT_PATH "${GEX_CTS_TOOLCHAIN}/aarch64-linux-musl")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)

# VK-GL-CTS otherwise guesses all 64-bit non-Android cross builds are x86-64.
set(DE_CPU DE_CPU_ARM_64 CACHE STRING "dEQP target CPU" FORCE)
