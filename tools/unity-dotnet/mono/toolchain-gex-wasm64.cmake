# CMake toolchain for code that runs as a gex stateful provider in an x86-64
# guest: clang for wasm64 against the musl wasm64 sysroot
# (tools/unity-dotnet/sysroot). Linux as far as the code can tell:
# its syscalls are x86-64 Linux's, served by gex-kernel.
#   -DGEX_SYSROOT=<OUT>/sysroot
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR wasm64)
set(CMAKE_C_COMPILER clang)
set(CMAKE_CXX_COMPILER clang++)
set(CMAKE_ASM_COMPILER clang)
set(CMAKE_AR llvm-ar)
set(CMAKE_RANLIB llvm-ranlib)
set(triple wasm64-unknown-unknown)
set(CMAKE_C_COMPILER_TARGET ${triple})
set(CMAKE_CXX_COMPILER_TARGET ${triple})
set(CMAKE_ASM_COMPILER_TARGET ${triple})
execute_process(COMMAND clang -print-resource-dir OUTPUT_VARIABLE resdir OUTPUT_STRIP_TRAILING_WHITESPACE)
set(gex_flags "-D__linux__=1 -D__linux=1 -D__unix__=1 -D__unix=1 -fPIC -matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint -mtail-call -fno-exceptions -Wno-unused-command-line-argument -nostdinc -isystem ${GEX_SYSROOT}/include -isystem ${resdir}/include")
set(CMAKE_C_FLAGS_INIT "${gex_flags}")
set(CMAKE_CXX_FLAGS_INIT "${gex_flags} -fno-rtti")
set(CMAKE_ASM_FLAGS_INIT "${gex_flags}")
set(CMAKE_EXE_LINKER_FLAGS_INIT "--no-wasm-opt -nostdlib -fuse-ld=lld ${GEX_SYSROOT}/lib/crt1.o -L${GEX_SYSROOT}/lib -lc ${GEX_SYSROOT}/lib/libclang_rt.builtins-wasm64.a -Wl,--import-memory,--shared-memory,--max-memory=17179869184,--no-entry,--export=__main_argc_argv")
set(CMAKE_FIND_ROOT_PATH ${GEX_SYSROOT})
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
# Checks that run their test program (try_run) cannot here: the answers for
# musl on gex-kernel.
set(HAVE_SHM_OPEN_THAT_WORKS_WELL_ENOUGH_WITH_MMAP_EXITCODE 1 CACHE STRING "" FORCE)
set(HAVE_CLOCK_REALTIME_EXITCODE 0 CACHE STRING "" FORCE)
set(HAVE_CLOCK_MONOTONIC_EXITCODE 0 CACHE STRING "" FORCE)
set(HAVE_CLOCK_MONOTONIC_COARSE_EXITCODE 0 CACHE STRING "" FORCE)
set(HAVE_CLOCK_GETTIME_NSEC_NP_EXITCODE 1 CACHE STRING "" FORCE)
