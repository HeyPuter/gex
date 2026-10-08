#!/bin/bash
# Build upstream Mono as a gex stateful provider (docs/unity-dotnet.md).
#
# The runtime is r58Playz/dotnet-runtime, branch wasm-jspi-jit-10.0.3 (dotnet/runtime v10.0.3 plus
# a runtime wasm JIT for Mono), at the commit below, with patches/ applied. Its CoreLib differs
# from upstream's (Reflection.Emit), so CoreLib is built from the same tree.
#
#   build.sh OUT [step...]
#
# Steps (default: src sysroot runtime native corelib provider):
#   src       clone the fork at COMMIT into OUT/src and apply patches/
#   sysroot   the wasm64 musl sysroot (tools/unity-dotnet/sysroot, musl from MUSL_SRC)
#   runtime   Mono for wasm64 under gex: clang + musl, Linux as far as Mono can tell (HOST_GEX),
#             interpreter only, coop suspend -> OUT/obj
#   native    libSystem.Native for the same target -> OUT/objnative
#   corelib   System.Private.CoreLib (linux-x64 Mono flavour) -> OUT/lib
#   formatters the out-of-band System.Runtime.Serialization.Formatters (NuGet, FORMATTERS_VERSION,
#             default 10.0.*): BinaryFormatter, which the shared framework only stubs since .NET 9 and
#             Unity games still use for saves -> OUT/lib-extra (tests/games/muck-dotnet.sh puts it in
#             the BCL directory, over the framework's stub)
#   provider  OUT/provider/mono.wasm: the runtime, System.Native, the adapters for every
#             public mono_* function it defines (host/gen-adapters.py, SysV x86-64) and the glue
#             (host/gexmono-provider.c), linked as a position-independent wasm64 module
#   em-runtime em-native em-smoke   P1's emscripten build and its node smoke test
#
# Needs clang/wasm-ld (22), llvm-ar, dotnet, git, ninja, cmake; /usr/lib/emsdk only for em-*.
# Builds at nice 19 with JOBS (default 12) jobs (stream rules).
set -euo pipefail

FORK=https://github.com/r58Playz/dotnet-runtime.git
BRANCH=wasm-jspi-jit-10.0.3
COMMIT=6812ea26ff228b8f4c132a7bad0b6a24c5653c2e

HERE=$(cd "$(dirname "$0")" && pwd)
UD=$(cd "$HERE/.." && pwd)
OUT=$(realpath -m "$1"); shift
STEPS=${*:-src sysroot runtime native corelib formatters provider}
MUSL_SRC=${MUSL_SRC:-$(realpath -m "$UD/../../../musl-wasm64")}
RUN="nice -n 19"
JOBS=${JOBS:-12}  # gooner rules: a heavy nice-19 build may use 12 jobs
# CMake for dotnet/runtime wants llvm-strings, llvm-readelf, llvm-link: from the versioned LLVM if not on PATH.
command -v llvm-strings >/dev/null || PATH=$PATH:/usr/lib/llvm-22/bin
SR=$OUT/sysroot/sysroot
TOOLCHAIN="-DCMAKE_TOOLCHAIN_FILE=$HERE/toolchain-gex-wasm64.cmake -DGEX_SYSROOT=$SR"
mkdir -p "$OUT"

step_src() {
	if [ ! -d "$OUT/src/.git" ]; then
		git clone -q --branch $BRANCH --shallow-since=2026-09-01 $FORK "$OUT/src"
	fi
	cd "$OUT/src"
	git checkout -q $COMMIT
	git checkout -q -- .
	for p in "$HERE"/patches/*.patch; do git apply "$p"; done
}

step_sysroot() {
	$RUN "$UD/sysroot/build.sh" "$OUT/sysroot" "$MUSL_SRC"
}

step_runtime() {
	mkdir -p "$OUT/obj"; cd "$OUT/obj"
	cmake -G Ninja $TOOLCHAIN -DCMAKE_BUILD_TYPE=Release -DGC_SUSPEND=coop \
		-DMONO_LIB_NAME=monosgen-2.0 -DMONO_SHARED_LIB_NAME=monosgen-2.0 -DDISABLE_SHARED_LIBS=1 -DDISABLE_EXECUTABLES=1 \
		-DENABLE_MINIMAL=sgen_split_nursery,sgen_gc_bridge,sgen_toggleref,sgen_binary_protocol,interpreter,qcalls,debugger_agent,log_dest \
		-DENABLE_INTERP_LIB=1 -DDISABLE_ICALL_TABLES=0 -DENABLE_ICALL_EXPORT=1 -DENABLE_LAZY_GC_THREAD_CREATION=1 \
		-DENABLE_LLVM_RUNTIME=1 -DSTATIC_COMPONENTS=1 -DMONO_COMPONENTS_RID=linux-x64 \
		-DFEATURE_PERFTRACING_DISABLE_PERFTRACING_LISTEN_PORTS=1 -DFEATURE_PERFTRACING_DISABLE_DEFAULT_LISTEN_PORT=1 \
		-DFEATURE_PERFTRACING_DISABLE_CONNECT_PORTS=1 -DWITH_OPTIM=OFF \
		"$OUT/src/src/mono" > "$OUT/runtime-configure.log"
	$RUN ninja -j"$JOBS" > "$OUT/runtime-build.log"
}

step_native() {
	# The BCL's native shim; no ICU (invariant globalization), Kerberos or OpenSSL.
	export __CMakeBinDir="$OUT/objnative/out"
	cmake -G Ninja -S "$OUT/src/src/native/libs" -B "$OUT/objnative" $TOOLCHAIN -DCMAKE_BUILD_TYPE=Release \
		-DCLR_CMAKE_TARGET_ARCH=wasm -DCLR_CMAKE_TARGET_OS=linux -DWITH_OPTIM=OFF -DGEX_NO_GLOBALIZATION_NATIVE=1 \
		> "$OUT/native-configure.log"
	cd "$OUT/objnative"
	$RUN ninja -j"$JOBS" System.Native/libSystem.Native.a > "$OUT/native-build.log"
}

step_formatters() {
	local W="$OUT/formatters-restore"
	rm -rf "$W"; mkdir -p "$W" "$OUT/lib-extra"
	cat > "$W/f.csproj" <<P
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>
  <ItemGroup><PackageReference Include="System.Runtime.Serialization.Formatters" Version="${FORMATTERS_VERSION:-10.0.*}" /></ItemGroup>
</Project>
P
	dotnet restore "$W/f.csproj" --packages "$W/pkgs" > "$OUT/formatters.log"
	cp "$(find "$W/pkgs/system.runtime.serialization.formatters" -path '*/lib/net[0-9]*/System.Runtime.Serialization.Formatters.dll' | sort -V | tail -1)" "$OUT/lib-extra/"
	echo "formatters: $(ls "$OUT/lib-extra")"
}

step_corelib() {
	cd "$OUT/src"
	$RUN ./build.sh -subset mono.corelib -os linux -arch x64 -c Release > "$OUT/corelib-build.log"
	mkdir -p "$OUT/lib"
	cp artifacts/bin/mono/linux.x64.Release/System.Private.CoreLib.dll "$OUT/lib/"
}

step_provider() {
	. "$UD/sysroot/flags.sh"
	local P="$OUT/src/src/native/public" N="$OUT/src/src/native" L="$OUT/obj/mono" W="${PROVIDER_DIR:-$OUT/provider}"
	mkdir -p "$W"; cd "$W"
	local INC="-nostdinc -isystem $SR/include -isystem $(clang -print-resource-dir)/include -I$P -I$OUT/src/src/mono -I$UD/provider -I$HERE/host -I$OUT/obj -I$OUT/src/src/mono/mono/eglib -I$OUT/obj/mono/eglib -I$OUT/src/src/native"
	llvm-nm --defined-only -j "$L"/mini/libmonosgen-2.0.a "$L"/mini/libmono-ee-interp.a "$L"/mini/libmono-component-*-stub-static.a \
		"$L"/mini/libmono-component-marshal-ilgen-static.a 2>/dev/null | grep '^mono_' | sort -u > have.txt
	# Everything written by hand (gexmono-provider.c's skip list, gexmono-unity.c's U(...)) is not generated.
	{ cat "$HERE/host/gexmono-skip.txt"; sed -n 's/^U(\(mono_[a-z0-9_]*\)).*/\1/p' "$HERE/host/gexmono-unity.c"; } > skip.txt
	python3 "$HERE/host/gen-adapters.py" --abi sysv --skip skip.txt --have have.txt \
		"$P"/mono/*/details/*-functions.h "$P"/mono/*/details/*-types.h > adapters.c 2> adapters.log
	python3 "$HERE/host/gen-m2n.py" "$HERE/host/m2n-extra.txt" > m2n.c
	for f in adapters.c m2n.c "$HERE/host/gexmono-provider.c" "$HERE/host/gexmono-unity.c" "$UD/provider/gexprov64.c" "$UD/provider/gexprov64-table.s" "$HERE/host/gexwj-stack.s"; do
		clang $WASM64_CFLAGS $INC -Wno-unused-command-line-argument -Wno-deprecated-declarations -c "$f" -o "$(basename "${f%.*}").o"
	done
	clang $WASM64_CFLAGS $INC -I"$N/libs/Common" -I"$OUT/objnative/Common" -I"$N/libs/System.Native" -I"$N" -I"$N/libs" \
		-c "$N/libs/System.Native/entrypoints.c" -o entrypoints-System.Native.o
	link() {
		wasm-ld -mwasm64 --experimental-pic -pie --no-entry --shared-memory --import-memory --max-memory=17179869184 -o mono.wasm \
				gexmono-provider.o gexmono-unity.o gexprov64.o gexprov64-table.o gexwj-stack.o adapters.o m2n.o entrypoints-System.Native.o "$OUT/objnative/System.Native/libSystem.Native.a" \
				"$L"/mini/libmonosgen-2.0.a "$L"/mini/libmono-ee-interp.a "$L"/mini/libmono-component-marshal-ilgen-static.a \
				"$L"/mini/libmono-component-hot_reload-stub-static.a "$L"/mini/libmono-component-debugger-stub-static.a \
				"$L"/mini/libmono-component-diagnostics_tracing-stub-static.a "$L"/mini/libmono-wasm-nosimd.a \
				"$L"/minipal/minipal/libminipal.a "$OUT/obj/_deps/fetchzlibng-build/libz.a" \
				-L"$SR/lib" -lc "$SR/lib/libclang_rt.builtins-wasm64.a" ${LINK_EXTRA:-}
	}
	# Interp-to-native wrappers for every function type in the provider (gen-m2n.py): link, read
	# the types, regenerate them, link again.
	link
	python3 "$HERE/host/gen-m2n.py" "$HERE/host/m2n-extra.txt" --types-from mono.wasm > m2n.c
	clang $WASM64_CFLAGS $INC -Wno-unused-command-line-argument -c m2n.c -o m2n.o
	link
	echo "provider: $W/mono.wasm; $(tail -1 adapters.log | cut -d, -f1)"
}

# ---- P1: the emscripten build and its node smoke test ----

step_em-runtime() {
	source /usr/lib/emsdk/emsdk_env.sh >/dev/null 2>&1
	local F="-m64 -pthread -fPIC"
	mkdir -p "$OUT/obj-em"; cd "$OUT/obj-em"
	emcmake cmake -G Ninja -DCMAKE_BUILD_TYPE=Release -DEMSCRIPTEN_SYSTEM_PROCESSOR=wasm64 -DCLR_CMAKE_HOST_ARCH=x64 \
		-DGC_SUSPEND=coop -DMONO_LIB_NAME=monosgen-2.0 -DMONO_SHARED_LIB_NAME=monosgen-2.0 \
		-DENABLE_MINIMAL=sgen_split_nursery,sgen_gc_bridge,sgen_toggleref,sgen_binary_protocol,logging,interpreter,qcalls,debugger_agent,log_dest \
		-DENABLE_INTERP_LIB=1 -DDISABLE_ICALL_TABLES=0 -DENABLE_ICALL_EXPORT=1 -DENABLE_LAZY_GC_THREAD_CREATION=1 \
		-DENABLE_WEBCIL=1 -DENABLE_LLVM_RUNTIME=1 -DSTATIC_COMPONENTS=1 -DMONO_COMPONENTS_RID=browser-wasm \
		-DFEATURE_PERFTRACING_PAL_WS=1 -DFEATURE_PERFTRACING_DISABLE_PERFTRACING_LISTEN_PORTS=1 \
		-DFEATURE_PERFTRACING_DISABLE_DEFAULT_LISTEN_PORT=1 \
		"-DCMAKE_ASM_FLAGS=$F" "-DCMAKE_C_FLAGS=$F -D_GNU_SOURCE=1 -mnontrapping-fptoint" \
		"-DCMAKE_CXX_FLAGS=$F -D_GNU_SOURCE=1 -mnontrapping-fptoint" "$OUT/src/src/mono" > "$OUT/em-runtime-configure.log"
	$RUN ninja -j"$JOBS" > "$OUT/em-runtime-build.log"
}

step_em-native() {
	source /usr/lib/emsdk/emsdk_env.sh >/dev/null 2>&1
	local F="-m64 -pthread -fPIC"
	export __CMakeBinDir="$OUT/objnative-em/out"
	emcmake cmake -G Ninja -S "$OUT/src/src/native/libs" -B "$OUT/objnative-em" -DCMAKE_BUILD_TYPE=Release \
		-DCLR_CMAKE_TARGET_OS=browser -DCLR_CMAKE_TARGET_ARCH=wasm -DCLR_CMAKE_HOST_ARCH=wasm -DMONO_WASM_MT=1 \
		"-DCMAKE_C_FLAGS=$F" "-DCMAKE_CXX_FLAGS=$F" > "$OUT/em-native-configure.log"
	cd "$OUT/objnative-em"
	$RUN ninja -j"$JOBS" System.Native/libSystem.Native.a > "$OUT/em-native-build.log"
	local N="$OUT/src/src/native"
	emcc $F -O2 -c "$N/libs/System.Native/entrypoints.c" -I"$N/libs/Common" -I"$OUT/objnative-em/Common" \
		-I"$N/libs/System.Native" -I"$N" -I"$N/libs" -o "$OUT/objnative-em/entrypoints-System.Native.o"
}

step_em-smoke() {
	exec "$UD/../../tests/unity-dotnet/smoke/run.sh" "$OUT"
}

for s in $STEPS; do
	echo "== $s"
	( "step_$s" )
done
