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
cc -O1 -rdynamic -o "$T/embed" "$HERE/embed.c" -ldl
dotnet build -c Release "$HERE/../smoke/t.csproj" -o "$T/asm" -p:BaseIntermediateOutputPath="$T/asmobj/" > "$T/asm.log"
FW=$(dirname "$(dotnet --list-runtimes | awk '/Microsoft.NETCore.App 10\./ {v=$2; p=$3} END {gsub(/[\[\]]/, "", p); print p "/" v "/x"}')")
cp "$OUT/lib/System.Private.CoreLib.dll" "$T/lib/"
cp "$FW/System.Runtime.dll" "$FW/System.Collections.dll" "$T/lib/"
cd "$T"
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout 300 "$GEXC" run --mem64 \
	--env GEXMONO_ASSEMBLIES="$T/lib" ./embed ./libmonobdwgc-2.0.so "$T/asm/t.dll" | tee out.txt
grep -q "^Compute(50) = 50002$" out.txt && grep -q "^Hello = hello gex from t ptr=8$" out.txt && grep -q "^Guest = twice 42; concat guest strings; mix 323.25; scale 2,4,6; big 11,25,52,3; caught from the guest$" out.txt && grep -q "^gc ok$" out.txt
echo PASS
