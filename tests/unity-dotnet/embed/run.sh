#!/bin/bash
# P2's test (docs/unity-dotnet.md): an x86-64 program embeds Mono the way
# UnityPlayer does (dlopen libmonobdwgc-2.0.so, dlsym the mono_* API), and
# under gex --mem64 the wasm64 Mono provider (tools/unity-dotnet/mono/build.sh
# OUT) serves the library: CoreLib boots on the interpreter, an assembly loads,
# methods run through mono_runtime_invoke, the GC collects. The library itself
# is a stub whose every export aborts.
#
#   run.sh OUT [gexc]
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath "$1")
GEXC=$(realpath "${2:-$HERE/../../../target/x86/release/gexc}")
T=$OUT/embed
mkdir -p "$T/lib"
cc -O1 -shared -fPIC -o "$T/libmonobdwgc-2.0.so" "$HERE/monostub.c"
cc -O1 -rdynamic -pthread -o "$T/embed" "$HERE/embed.c" -ldl
dotnet build -c Release "$HERE/../smoke/t.csproj" -o "$T/asm" -p:BaseIntermediateOutputPath="$T/asmobj/" > "$T/asm.log"
FW=$(dirname "$(dotnet --list-runtimes | awk '/Microsoft.NETCore.App 10\./ {v=$2; p=$3} END {gsub(/[\[\]]/, "", p); print p "/" v "/x"}')")
cp "$OUT/lib/System.Private.CoreLib.dll" "$T/lib/"
cp "$FW/System.Runtime.dll" "$FW/System.Collections.dll" "$FW/System.Runtime.InteropServices.dll" "$T/lib/"
cd "$T"
# The interpreter only (the wasm JIT is on by default; GEXMONO_WASM_JIT=0).
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout 300 "$GEXC" run --mem64 --env GEXMONO_WASM_JIT=0 \
	--env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" | tee out.txt
PATTERNS=("Compute(50) = 50002" "Hello = hello gex from t ptr=8"
	"Guest = twice 42; concat guest strings; mix 323.25; scale 2,4,6; big 11,25,52,3; caught from the guest"
	"Back = callback 806.50; unmanaged 1015" "Work(200) = 24983091"
	"held across collections: 64 of 64 intact" "worker thread: 0 bad of 640"
	"clears 643296" "delegates 24660 tag0tag1tag2" "checked .OOO.O.O.O..O.O.O... .OOO.O.O.O..O.O.O..." "argaddr 2106683" "hotshapes 3509709 10437154840.67" "monoicalls 699466" "lcsewide 8531659618475" "readfloats 7507799.7418" "native 914124880 caught directly caught directly caught directly" "finally 654888186 505936 334" "reflect 449323311 1225" "localloc 1139912260192 aaaaaaaaaaaaaabpcodhiyunocapbocnjOswdcoacoemgOOwyacoaocmekOOusqocadngkjOOucnqaahjOOOOOOOOOOaiOOOOOOOOOOOayswuuOOeiaysauwyscOOiOOOOandaqnOOaOnaqaoccoqOOyOaOOacoocaOOsOqOO aaaaaaaaaabcdphvwsacegOoqskadgjOOlocapOOOOOOOahoOOOOOOavqlOOzuOawsoOOuOOaskcOOOOO" "usedsize grew stable shrank" "liveness: 301 leaves, 0 reported twice, batched" "gc ok")
for p in "${PATTERNS[@]}"; do grep -a -q -x -F "$p" out.txt || { echo "FAIL: no '$p'"; exit 1; }; done
# The same under the function JIT, which must leave the replaced library's own code alone: it once
# followed the PLT into the stub ("x86 Mono stub ... called"), as it followed Muck's into Boehm's GC.
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout 300 "$GEXC" run --mem64 --fjit \
	--env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" > fjit.txt 2>&1 || true
if ! grep -q "^gc ok$" fjit.txt; then echo "FAIL (--fjit)"; tail -3 fjit.txt; exit 1; fi
# The fork's wasm JIT (GEXMONO_WASM_JIT=1) at threshold 1: every method it accepts runs as a
# wasm64 module compiled by the host, and the results must be the interpreter's.
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout 600 "$GEXC" run --mem64 \
	--env GEXMONO_WASM_JIT=1 --env MONO_WASM_JIT_THRESHOLD=1 --env MONO_WASM_JIT_VERBOSE=1 \
	--env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" > wasmjit.txt 2>&1 || true
for p in "${PATTERNS[@]}"; do grep -a -q -x -F "$p" wasmjit.txt || { echo "FAIL (wasm JIT): no '$p'"; exit 1; }; done
if grep -a -q '^WASM_JIT_INVALID' wasmjit.txt; then echo "FAIL (wasm JIT): a module did not validate"; grep -a '^WASM_JIT_INVALID' wasmjit.txt | head -3 | cut -c1-200; exit 1; fi
# Threshold 10 compiles in a different order (callees before callers, tier-2 recompiles), which once put
# a virtual call's i32 result into an i64 register (UTF8Encoding.GetChars). Same results, and every
# emitted module must validate (MONO_WASM_JIT_VERBOSE=1 names each one that does not).
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout 600 "$GEXC" run --mem64 \
	--env GEXMONO_WASM_JIT=1 --env MONO_WASM_JIT_THRESHOLD=10 --env MONO_WASM_JIT_VERBOSE=1 \
	--env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" > wasmjit10.txt 2>&1 || true
for p in "${PATTERNS[@]}"; do grep -a -q -x -F "$p" wasmjit10.txt || { echo "FAIL (wasm JIT, threshold 10): no '$p'"; exit 1; }; done
if grep -a -q '^WASM_JIT_INVALID' wasmjit10.txt; then echo "FAIL (wasm JIT, threshold 10): a module did not validate"; grep -a '^WASM_JIT_INVALID' wasmjit10.txt | head -3 | cut -c1-200; exit 1; fi
# The worker thread under load (EMBED_WORKER_STRESS: 10 times its rounds and the main thread's
# collections): a provider call's result in rax was overwritten by a syscall in the runtime's GC-safe
# transition on the way back to the guest (a futex while another thread collected), so the worker
# saw NULL from Make and from mono_string_to_utf8 in about 1 run in 20 (every run under this load);
# then, kept only in a wasm local across that wait, the result was freed and its slot reused.
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout 600 "$GEXC" run --mem64 \
	--env GEXMONO_WASM_JIT=1 --env MONO_WASM_JIT_THRESHOLD=2 --env EMBED_WORKER_STRESS=10 \
	--env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" > stress.txt 2>&1 || true
grep -a -q -x -F "worker thread: 0 bad of 640" stress.txt || { echo "FAIL (worker under load)"; grep -a '^worker' stress.txt | head -3; exit 1; }
# The liveness walk as it was before the class shapes (field metadata per object): the same set.
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout 300 "$GEXC" run --mem64 --env GEXMONO_LV_NOSHAPE=1 \
	--env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" > noshape.txt 2>&1 || true
grep -a -q -x -F "liveness: 301 leaves, 0 reported twice, batched" noshape.txt || { echo "FAIL (GEXMONO_LV_NOSHAPE)"; exit 1; }
# The control: without scanning the guest's frames, objects held only there die.
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout 300 "$GEXC" run --mem64 --env GEXP_NO_GUEST_ROOTS=1 \
	--env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" > control.txt 2>&1 || true
if grep -q "^held across collections: 64 of 64 intact$" control.txt; then echo "FAIL (the control kept its objects)"; exit 1; fi
echo PASS
