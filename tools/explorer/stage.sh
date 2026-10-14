#!/bin/bash
# Stage the real Windows 7 shell pieces into a W3 Wine prefix.
#
#   stage.sh IMG TEMPLATE_PFX OUT_PFX
#
# IMG is an extracted Windows 7 x86 image (the root holding Windows/, e.g.
# 7zz x install.wim). Nothing from it is executed here, and nothing from it
# goes into git. OUT_PFX is a copy of TEMPLATE_PFX with:
#
# - the native images in NATIVE (env; default: the theme set) in system32,
#   unmodified, each with its en-US .mui in system32/en-US (the Wine fork's
#   loader looks resources up there, as Windows' does);
# - Windows/Resources (Themes, and Ease of Access Themes, which holds the
#   Windows 7 Basic and Classic themes), as is: a visual style carries a
#   signature over its bytes (uxtheme and themeservice refuse an unsigned
#   one), so its .mui cannot be merged in;
# - Windows/Fonts (all, or the FONTS globs);
# - the registry Windows 7 has for them, copied from IMG's hives:
#   the Themes service (svchost netsvcs, themeservice.dll), the svchost
#   groups, the theme manager's machine settings, and the COM classes the
#   native images serve;
# - DllOverrides for the native images, so every process (services too)
#   loads them, and Wine's Windows version set to Windows 7.
#
# native-explorer.txt holds the set for explorer.exe folder windows:
#   NATIVE="$(cat native-explorer.txt)" stage.sh IMG TPL OUT
#
# --overrides prints only the WINEDLLOVERRIDES string.
set -eu
T=$(cd "$(dirname "$0")" && pwd)
NATIVE=${NATIVE:-"uxtheme.dll themeservice.dll UXInit.dll"}
ovr() { for n in $NATIVE; do b=${n%.*}; echo -n "${b,,}=n;"; done | sed 's/;$//'; }
if [ "${1:-}" = --overrides ]; then ovr; echo; exit 0; fi

IMG=$(cd "$1" && pwd); TPL=$2; OUT=$3
S=$IMG/Windows/System32
REG="python3 -I $T/hive-reg.py"
# python-registry (docs/win7-shell.md: pip install --target DIR python-registry==1.3.1)
export REGISTRY_LIB=${REGISTRY_LIB:?set REGISTRY_LIB to a directory holding python-registry 1.3.1}

chmod -R u+w "$OUT" 2>/dev/null || true; rm -rf "$OUT"; cp -a "$TPL" "$OUT"
D=$OUT/drive_c/windows

# a file from IMG, case-insensitively (Windows names are not consistently cased)
src() { find "$1" -maxdepth 1 -iname "$2" -print -quit; }

put() {  # put IMAGE_DIR NAME DEST_DIR
  local f m; f=$(src "$1" "$2"); [ -n "$f" ] || { echo "stage: no $2 in $1" >&2; exit 1; }
  m=$(src "$1/en-US" "$(basename "$f").mui")
  mkdir -p "$3"; rm -f "$3/$(basename "$f")" "$3/${2,,}"
  cp "$f" "$3/"
  if [ -n "$m" ]; then mkdir -p "$3/en-US"; cp "$m" "$3/en-US/"; fi
}

for n in $NATIVE; do
  case ${n,,} in
    explorer.exe)  # the shell, in the Windows directory (system32's stays Wine's: the desktop)
      put "$IMG/Windows" "$n" "$D" ;;
    ieproxy.dll)   # Internet Explorer's proxies: IServiceProvider and other shell interfaces
      put "$IMG/Program Files/Internet Explorer" "$n" "$OUT/drive_c/Program Files/Internet Explorer" ;;
    comctl32.dll)  # v6 from SxS replaces the file in Wine's v6 assembly; v5 stays Wine's
      sxs=$(ls -d "$IMG"/Windows/winsxs/x86_microsoft.windows.common-controls_6595b64144ccf1df_6.* | tail -1)
      mui=$(ls -d "$IMG"/Windows/winsxs/x86_microsoft.windows.c*-controls.resources_6595b64144ccf1df_6.*_en-us_* | tail -1)
      for d in "$D"/winsxs/x86_microsoft.windows.common-controls_6595b64144ccf1df_6.*; do
        rm -f "$d"/[Cc][Oo][Mm][Cc][Tt][Ll]32.dll; cp "$sxs/comctl32.dll" "$d/"
        mkdir -p "$d/en-US"; cp "$mui/comctl32.dll.mui" "$d/en-US/"
      done ;;
    *) put "$S" "$n" "$D/system32" ;;
  esac
done

# themes and visual styles (the template's own out of the way)
rm -rf "$D/resources"; mkdir -p "$D/resources"
cp -r "$IMG/Windows/Resources/." "$D/resources/"

# Windows' branding (the edition's name and logos, which winbrand.dll reads)
if [[ " ${NATIVE,,} " == *" winbrand.dll "* ]]; then
  rm -rf "$D/branding"; mkdir -p "$D/branding"; cp -r "$IMG/Windows/Branding/." "$D/branding/"
fi

# the desktop backgrounds the themes name (%SystemRoot%\Web\Wallpaper)
rm -rf "$D/web"; mkdir -p "$D/web"; cp -r "$IMG/Windows/Web/." "$D/web/"

# the account pictures (ProgramData\Microsoft\User Account Pictures): the
# shell shows user.bmp (guest.bmp for a guest) as the tile of an account that
# has none of its own
pics="$IMG/ProgramData/Microsoft/User Account Pictures"
if [ -d "$pics" ]; then
  rm -rf "$OUT/drive_c/ProgramData/Microsoft/User Account Pictures"
  mkdir -p "$OUT/drive_c/ProgramData/Microsoft"
  cp -r "$pics" "$OUT/drive_c/ProgramData/Microsoft/"
fi

# the stock programs and the Start menu's shortcuts (stage-stock.sh)
"$T/stage-stock.sh" "$IMG" "$OUT"

# Windows' fonts (Segoe UI and the rest the shell and the theme use; Wine
# finds them in C:\windows\Fonts); FONTS (env) narrows them to a glob list
mkdir -p "$D/Fonts"
(set -f; for g in ${FONTS:-*}; do find "$IMG/Windows/Fonts" -maxdepth 1 -type f -iname "$g" -exec cp {} "$D/Fonts/" \;; done)

H=$S/config
$REG "$OUT" \
  "$H/SYSTEM:ControlSet001\\Services\\Themes" \
  "$H/SOFTWARE:Microsoft\\Windows NT\\CurrentVersion\\Svchost=Software\\Microsoft\\Windows NT\\CurrentVersion\\Svchost" \
  "$H/SOFTWARE:Microsoft\\Windows\\CurrentVersion\\ThemeManager=Software\\Microsoft\\Windows\\CurrentVersion\\ThemeManager" \
  "$H/SOFTWARE:Microsoft\\Windows\\CurrentVersion\\Themes=Software\\Microsoft\\Windows\\CurrentVersion\\Themes"
# the default user's window metrics (Wine's own defaults out of the way)
$REG "$OUT" "$IMG/Users/Default/NTUSER.DAT:Control Panel\\Desktop\\WindowMetrics@user"
# the COM classes the native images serve
$REG --com "$H/SOFTWARE" "$OUT" $NATIVE
# the shell's namespace and file-type settings: folder, drive and file
# verbs (explorer opens a folder by its Folder verbs), the program
# extensions' types (Wine has no HKCR\.exe: without it the shell finds no
# handler for a program and starts none), known folders,
# folder types, the namespace roots, and the property system's schemas
SH=()
for k in Folder Directory Drive AllFilesystemObjects '*' Unknown lnkfile exefile SystemFileAssociations DesktopBackground \
         .exe .com .bat .cmd .lnk; do
  SH+=("$H/SOFTWARE:Classes\\$k=Software\\Classes\\$k")
done
for k in Explorer PropertySystem 'Shell Extensions'; do
  SH+=("$H/SOFTWARE:Microsoft\\Windows\\CurrentVersion\\$k=Software\\Microsoft\\Windows\\CurrentVersion\\$k")
done
# the shell windows list: Windows registers it with no server (the shell
# serves it, CoRegisterClassObject), Wine with an in-process one that has
# none to give (ieframe), which COM then never looks past
for k in 'CLSID\{9BA05972-F6A8-11CF-A442-00A0C90A8F39}' 'AppID\{9BA05972-F6A8-11CF-A442-00A0C90A8F39}'; do
  SH+=("$H/SOFTWARE:Classes\\$k=Software\\Classes\\$k!")
done
# the namespace roots replace Wine's (its Unix file system root on the
# desktop and in Computer), and their classes are registered as Windows has
# them, whether or not their DLLs are staged (the shell reads which items
# are hidden from the class)
for k in 'Desktop\NameSpace' 'MyComputer\NameSpace'; do
  SH+=("$H/SOFTWARE:Microsoft\\Windows\\CurrentVersion\\Explorer\\$k=Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\$k!")
done
$REG "$OUT" "${SH[@]}"
$REG --ns "$H/SOFTWARE" "$OUT" 'Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace' \
  'Microsoft\Windows\CurrentVersion\Explorer\MyComputer\NameSpace'

# every process, services included, loads the native images; Wine reports
# Windows 7, whose components these are (on it, with the Basic theme, DWM
# composition is off, which is what dwmapi then reports)
python3 -I $T/user-reg.py "$OUT/user.reg" 'Software\Wine\DllOverrides' \
  $(for n in $NATIVE; do b=${n%.*}; case ${n,,} in
      explorer.exe) ;;                         # by path: system32's is Wine's
      comctl32.dll) echo "${b,,}=native,builtin" ;;  # v5 (system32) stays Wine's
      *) echo "${b,,}=native" ;; esac; done) \
  -- 'Software\Wine' Version=win7
# Direct3D on WebGPU (wined3d's WebGPU renderer, /dev/gex-gpu with the
# page's ?webgpu=1; docs/d3d-webgpu.md): the page has no OpenGL for the
# default renderer, and a Wine without that renderer ignores the name
python3 -I $T/user-reg.py "$OUT/user.reg" 'Software\Wine\Direct3D' renderer=webgpu
# explorer.exe opens folder windows in a process of its own (Wine's explorer
# owns the desktop, and is not a shell that could host them)
python3 -I $T/user-reg.py "$OUT/user.reg" 'Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced' 'SeparateProcess=dword:00000001'
# Windows gives a service 120 s to start (the Themes service's svchost and
# the COM servers start slowly on a cold cache), and the mount manager, which
# the shell's drive and volume objects query, starts at boot as on Windows
python3 -I $T/user-reg.py "$OUT/system.reg" 'System\ControlSet001\Control' 'ServicesPipeTimeout=120000' \
  -- 'System\ControlSet001\Services\MountMgr' 'Start=dword:00000000'
# SHELL_KIND=windows: Windows' explorer is the session's shell (Winlogon's Shell
# names it, so Wine's desktop process leaves it the shell's roles)
if [ "${SHELL_KIND:-}" = windows ]; then
  python3 -I $T/user-reg.py "$OUT/system.reg" 'Software\Microsoft\Windows NT\CurrentVersion\Winlogon' 'Shell=C:\\Windows\\explorer.exe'
  # the Basic theme's background, as its .theme sets it (Windows' shell
  # draws it; Wine's desktop, which drew none, is not the shell)
  python3 -I $T/user-reg.py "$OUT/user.reg" 'Control Panel\Desktop' 'Wallpaper=C:\\Windows\\web\\wallpaper\\Windows\\img0.jpg' \
    'WallpaperStyle=10' 'TileWallpaper=0'
fi
echo "staged $OUT ($NATIVE)"
