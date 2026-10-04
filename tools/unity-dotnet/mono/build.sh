#!/bin/bash
# Build upstream Mono for wasm64 (memory64) for gex: docs/unity-dotnet.md.
#
# The runtime is r58Playz/dotnet-runtime, branch wasm-jspi-jit-10.0.3 (dotnet/runtime v10.0.3 plus
# a runtime wasm JIT for Mono), at the commit below, with patches/ applied. Its CoreLib differs
# from upstream's (Reflection.Emit), so CoreLib is built from the same tree.
#
#   build.sh OUT [step...]      steps: src runtime native corelib smoke (default: all)
#
# OUT gets: src/ (the patched tree), obj/ (the runtime: libmonosgen-2.0.a, libmono-ee-interp.a, ...),
# objnative/ (libSystem.Native.a), lib/ (System.Private.CoreLib.dll), smoke/.
# Needs /usr/lib/emsdk (emscripten with the wasm64 sysroot), dotnet, git, ninja, node >= 24
# (memory64 without a flag) for the smoke test. Builds on cores 0,2,6,8 at nice 19 (stream rules).
set -euo pipefail

FORK=https://github.com/r58Playz/dotnet-runtime.git
BRANCH=wasm-jspi-jit-10.0.3
COMMIT=6812ea26ff228b8f4c132a7bad0b6a24c5653c2e

HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath -m "$1"); shift
STEPS=${*:-src runtime native corelib smoke}
RUN="taskset -c 0,2,6,8 nice -n 19"
mkdir -p "$OUT"
source /usr/lib/emsdk/emsdk_env.sh >/dev/null 2>&1
F="-m64 -pthread -fPIC"

step_src() {
	if [ ! -d "$OUT/src/.git" ]; then
		git clone -q --branch $BRANCH --shallow-since=2026-09-01 $FORK "$OUT/src"
	fi
	cd "$OUT/src"
	git checkout -q $COMMIT
	git checkout -q -- .
	for p in "$HERE"/patches/*.patch; do git apply "$p"; done
}

step_runtime() {
	mkdir -p "$OUT/obj"; cd "$OUT/obj"
	# The browser's configuration (src/mono/mono.proj) as the fork has it, with threads, the icall
	# tables linked in, coop suspend (mono-threads-wasm requires it with threads) and wasm64.
	emcmake cmake -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$OUT/obj/out" -DCMAKE_INSTALL_LIBDIR=lib \
		-DEMSCRIPTEN_SYSTEM_PROCESSOR=wasm64 -DCLR_CMAKE_HOST_ARCH=x64 -DGC_SUSPEND=coop \
		-DMONO_LIB_NAME=monosgen-2.0 -DMONO_SHARED_LIB_NAME=monosgen-2.0 \
		-DENABLE_MINIMAL=sgen_split_nursery,sgen_gc_bridge,sgen_toggleref,sgen_binary_protocol,logging,interpreter,qcalls,debugger_agent,log_dest \
		-DENABLE_INTERP_LIB=1 -DDISABLE_ICALL_TABLES=0 -DENABLE_ICALL_EXPORT=1 -DENABLE_LAZY_GC_THREAD_CREATION=1 \
		-DENABLE_WEBCIL=1 -DENABLE_LLVM_RUNTIME=1 -DSTATIC_COMPONENTS=1 -DMONO_COMPONENTS_RID=browser-wasm \
		-DFEATURE_PERFTRACING_PAL_WS=1 -DFEATURE_PERFTRACING_DISABLE_PERFTRACING_LISTEN_PORTS=1 \
		-DFEATURE_PERFTRACING_DISABLE_DEFAULT_LISTEN_PORT=1 \
		"-DCMAKE_ASM_FLAGS=$F" "-DCMAKE_C_FLAGS=$F -D_GNU_SOURCE=1 -mnontrapping-fptoint" \
		"-DCMAKE_CXX_FLAGS=$F -D_GNU_SOURCE=1 -mnontrapping-fptoint" "$OUT/src/src/mono" > "$OUT/runtime-configure.log"
	$RUN ninja -j4 > "$OUT/runtime-build.log"
}

step_native() {
	# The BCL's native shims, as the browser builds them (no ICU: globalization is invariant).
	export __CMakeBinDir="$OUT/objnative/out"
	emcmake cmake -G Ninja -S "$OUT/src/src/native/libs" -B "$OUT/objnative" -DCMAKE_BUILD_TYPE=Release \
		-DCLR_CMAKE_TARGET_OS=browser -DCLR_CMAKE_TARGET_ARCH=wasm -DCLR_CMAKE_HOST_ARCH=wasm -DMONO_WASM_MT=1 \
		"-DCMAKE_C_FLAGS=$F" "-DCMAKE_CXX_FLAGS=$F" > "$OUT/native-configure.log"
	cd "$OUT/objnative"
	$RUN ninja -j4 System.Native/libSystem.Native.a > "$OUT/native-build.log"
	local N="$OUT/src/src/native"
	emcc $F -O2 -c "$N/libs/System.Native/entrypoints.c" -I"$N/libs/Common" -I"$OUT/objnative/Common" \
		-I"$N/libs/System.Native" -I"$N" -I"$N/libs" -o "$OUT/objnative/entrypoints-System.Native.o"
}

step_corelib() {
	cd "$OUT/src"
	$RUN ./build.sh -subset mono.corelib -os linux -arch x64 -c Release > "$OUT/corelib-build.log"
	mkdir -p "$OUT/lib"
	cp artifacts/bin/mono/linux.x64.Release/System.Private.CoreLib.dll "$OUT/lib/"
}

step_smoke() {
	exec "$HERE/../../../tests/unity-dotnet/smoke/run.sh" "$OUT"
}

for s in $STEPS; do
	echo "== $s"
	( step_$s )
done
