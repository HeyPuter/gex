#!/bin/bash
# Smoke test of the wasm64 Mono (tools/unity-dotnet/mono/build.sh OUT): boots CoreLib on the
# interpreter, loads t.dll, looks up a class and methods, registers an internal call, calls
# mono_runtime_invoke (List, Dictionary, int.ToString, GC.Collect, the icall; string concat and
# reflection), and collects. Runs under node (V8, memory64); a correctness check only.
#
#   run.sh OUT
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
MONO=$HERE/../../../tools/unity-dotnet/mono
OUT=$(realpath "$1")
T=$OUT/smoke
mkdir -p "$T/lib"
source /usr/lib/emsdk/emsdk_env.sh >/dev/null 2>&1
F="-m64 -pthread -O1"
INC="-I$OUT/src/src/native/public -I$OUT/obj"

dotnet build -c Release "$HERE/t.csproj" -o "$T/asm" -p:BaseIntermediateOutputPath="$T/asmobj/" > "$T/asm.log"
# CoreLib from the fork's tree; the facades t.dll references from the installed .NET 10.
FW=$(dirname "$(dotnet --list-runtimes | awk '/Microsoft.NETCore.App 10\./ {v=$2; p=$3} END {gsub(/[\[\]]/, "", p); print p "/" v "/x"}')")
cp "$OUT/lib/System.Private.CoreLib.dll" "$T/lib/"
cp "$FW/System.Runtime.dll" "$FW/System.Collections.dll" "$T/lib/"

python3 "$MONO/host/gen-m2n.py" "$MONO/host/m2n-extra.txt" > "$T/m2n.c"
emcc $F -c "$T/m2n.c" -o "$T/m2n.o"
emcc $F -c "$MONO/host/gexmono.c" $INC -o "$T/gexmono.o"
emcc $F -c "$HERE/main.c" $INC -o "$T/main.o"
L=$OUT/obj/mono
emcc $F "$T/main.o" "$T/m2n.o" "$T/gexmono.o" \
	"$OUT/objnative/entrypoints-System.Native.o" "$OUT/objnative/System.Native/libSystem.Native.a" \
	$L/mini/libmonosgen-2.0.a $L/mini/libmono-ee-interp.a \
	$L/mini/libmono-component-marshal-ilgen-static.a $L/mini/libmono-component-hot_reload-stub-static.a \
	$L/mini/libmono-component-debugger-stub-static.a $L/mini/libmono-component-diagnostics_tracing-stub-static.a \
	$L/mini/libmono-wasm-eh-wasm.a $L/mini/libmono-wasm-nosimd.a $L/minipal/minipal/libminipal.a \
	"$OUT/obj/_deps/fetchzlibng-build/libz.a" \
	-fwasm-exceptions -sNODERAWFS=1 -sEXIT_RUNTIME=1 -sALLOW_MEMORY_GROWTH=1 -sINITIAL_MEMORY=256MB \
	-sMAXIMUM_MEMORY=8GB -sSTACK_SIZE=8MB -sERROR_ON_UNDEFINED_SYMBOLS=0 --profiling-funcs \
	-o "$T/smoke.js" > "$T/link.log" 2>&1

# The process does not exit by itself yet (mono's threads keep the emscripten pthreads runtime
# alive; exit() does not end it), so wait for the last line or a failure, then stop it by PID.
node "$T/smoke.js" "$T/lib" "$T/asm/t.dll" > "$T/out.txt" 2>&1 &
pid=$!
for _ in $(seq 240); do
	grep -q "^gc ok\|threw\|Abort\|Error" "$T/out.txt" && break
	kill -0 $pid 2>/dev/null || break
	sleep 0.5
done
kill $pid 2>/dev/null || true
cat "$T/out.txt"
grep -q "Run(50) = 50002" "$T/out.txt" && grep -q "Hello = hello gex from t ptr=8" "$T/out.txt" && grep -q "^gc ok" "$T/out.txt"
echo PASS
