#!/bin/sh
# A diagnostic assembly for a Unity Mono game under gex (docs/unity-dotnet.md):
# Probe.cs logs every first-chance exception and, every 5 s of real time, the
# clock, the "StartManager" object's animator and components, and the cameras.
# Unity runs it through RuntimeInitializeOnLoads.json; no scene changes.
#
#   build.sh GAME_DATA_DIR OVERLAY_DIR
#
# GAME_DATA_DIR: the game's <name>_Data. Writes OVERLAY_DIR/<name>_Data/{Managed/
# GexProbe.dll,ScriptingAssemblies.json,RuntimeInitializeOnLoads.json} for
# tests/games/muck-dotnet.sh's OVERLAY= (the game's own files stay untouched).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
D=$(realpath "$1"); O=$(realpath -m "$2"); N=$(basename "$D")
W=$O.build; mkdir -p "$W" "$O/$N/Managed"
cp "$HERE/Probe.cs" "$W/"
cat > "$W/GexProbe.csproj" <<P
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>netstandard2.1</TargetFramework>
    <AssemblyName>GexProbe</AssemblyName>
    <EnableDefaultCompileItems>false</EnableDefaultCompileItems>
  </PropertyGroup>
  <ItemGroup>
    <Compile Include="Probe.cs" />
    <Reference Include="UnityEngine.CoreModule"><HintPath>$D/Managed/UnityEngine.CoreModule.dll</HintPath><Private>false</Private></Reference>
    <Reference Include="UnityEngine.AnimationModule"><HintPath>$D/Managed/UnityEngine.AnimationModule.dll</HintPath><Private>false</Private></Reference>
    <Reference Include="UnityEngine.TextRenderingModule"><HintPath>$D/Managed/UnityEngine.TextRenderingModule.dll</HintPath><Private>false</Private></Reference>
    <Reference Include="UnityEngine.InputLegacyModule"><HintPath>$D/Managed/UnityEngine.InputLegacyModule.dll</HintPath><Private>false</Private></Reference>
  </ItemGroup>
</Project>
P
dotnet build -c Release -o "$W/out" "$W/GexProbe.csproj" > "$W/build.log" || { tail -20 "$W/build.log"; exit 1; }
cp "$W/out/GexProbe.dll" "$O/$N/Managed/"
python3 - "$D" "$O/$N" <<'PY'
import json, sys
d, o = sys.argv[1:]
a = json.load(open(d + '/ScriptingAssemblies.json'))
a['names'].append('GexProbe.dll'); a['types'].append(16)
json.dump(a, open(o + '/ScriptingAssemblies.json', 'w'))
r = json.load(open(d + '/RuntimeInitializeOnLoads.json'))
r['root'].insert(0, {"assemblyName": "GexProbe", "nameSpace": "", "className": "GexProbe",
                     "methodName": "Init", "loadTypes": 1, "isUnityClass": False})
json.dump(r, open(o + '/RuntimeInitializeOnLoads.json', 'w'))
PY
echo "overlay: $O"
