#!/bin/bash
# The Start menu's shortcuts and the programs behind a new user's stock
# taskbar pins and most-used list, from a Windows 7 image into a prefix
# (in place; stage.sh runs it, and it brings an already staged prefix up
# to date).
#
#   stage-stock.sh IMG PFX
#
# IMG is an extracted Windows 7 x86 image (the root holding Windows/). As in
# stage.sh, nothing from it is executed here, and nothing from it goes into
# git. Explorer seeds a new user's taskbar pins (Internet Explorer, Windows
# Explorer, Windows Media Player) and most-used list (Getting Started, Media
# Center, Calculator, Sticky Notes, Snipping Tool, Paint, Remote Desktop
# Connection, Magnifier, Solitaire) from the Start menu's shortcuts by
# name, the first time it runs for that user; each needs its shortcut and
# that shortcut's program.
set -eu
T=$(cd "$(dirname "$0")" && pwd)
IMG=$(cd "$1" && pwd); OUT=$2
S=$IMG/Windows/System32
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

# The programs behind a new user's stock taskbar pins and most-used list
# (STOCK, env: system32 images, each with its .mui; STOCK_DIRS: whole
# directories of IMG, over the template's): their shortcuts stay (the prune
# below keeps a link only when its program is here) and show the programs'
# own icons and names, whether or not a program starts yet.
STOCK=${STOCK-"OobeFldr.dll calc.exe mspaint.exe MFC42u.dll StikyNot.exe SNTSearch.dll SnippingTool.exe Magnify.exe Magnification.dll mstsc.exe mstscax.dll iertutil.dll urlmon.dll"}
STOCK_DIRS=${STOCK_DIRS-"Program Files/Internet Explorer:Program Files/Windows Media Player:Program Files/Microsoft Games:Windows/ehome"}
for n in $STOCK; do put "$S" "$n" "$D/system32"; done
# system32 files the image has only in its component store (winsxs), which
# setup projects when it turns the component on: the card games' common
# library (Solitaire: "Unable to find the file [CardGames.dll]") and Paint's
# Ribbon (Wine's uiribbon has no image factory: CLASS_E_CLASSNOTAVAILABLE for
# {0F7434B6-...}, and Paint's window callback faults), each with its en-US
# .mui from the resources component when there is one
STOCK_SXS=${STOCK_SXS-"CardGames.dll UIRibbon.dll UIRibbonRes.dll"}
for n in $STOCK_SXS; do
  [ -n "$(src "$S" "$n")" ] && { put "$S" "$n" "$D/system32"; continue; }
  f=$(find "$IMG/Windows/winsxs" -mindepth 2 -maxdepth 2 -path '*/x86_*' -iname "$n" | sort | tail -1)
  [ -n "$f" ] || { echo "stage: no $n in the image" >&2; exit 1; }
  rm -f "$D/system32/$n" "$D/system32/${n,,}"; cp "$f" "$D/system32/"
  m=$(find "$IMG/Windows/winsxs" -mindepth 2 -maxdepth 2 -path '*/x86_*.resources_*_en-us_*' -iname "$n.mui" | sort | tail -1)
  if [ -n "$m" ]; then mkdir -p "$D/system32/en-US"; rm -f "$D/system32/en-US/$(basename "$m")"; cp "$m" "$D/system32/en-US/"; fi
done
# GDI+ for Paint: Windows 7's, an SxS assembly, in system32 and over Wine's
# assembly copies (Paint as P's mspaint stage.sh runs it)
g=$(ls "$IMG"/Windows/winsxs/x86_microsoft.windows.gdiplus_6595b64144ccf1df_1.1.*/[Gg]di[Pp]lus.dll 2>/dev/null | tail -1)
if [ -n "$g" ]; then
  rm -f "$D/system32/gdiplus.dll"; cp "$g" "$D/system32/gdiplus.dll"
  for a in "$D"/winsxs/x86_microsoft.windows.gdiplus_6595b64144ccf1df_1.1.*; do
    [ -d "$a" ] && { rm -f "$a/gdiplus.dll"; cp "$g" "$a/gdiplus.dll"; }
  done
fi
# The images above that replace Wine's own load natively (STOCK_NATIVE:
# module names for DllOverrides)
STOCK_NATIVE=${STOCK_NATIVE-"uiribbon uiribbonres gdiplus mfc42u"}
[ -z "$STOCK_NATIVE" ] || python3 -I "$T/user-reg.py" "$OUT/user.reg" 'Software\Wine\DllOverrides' \
  $(for n in $STOCK_NATIVE; do echo "$n=native"; done)
IFS=: read -r -a stock_dirs <<< "$STOCK_DIRS"
for d in "${stock_dirs[@]}"; do
  [ -n "$d" ] || continue
  [ -d "$IMG/$d" ] || { echo "stage: no $d in the image" >&2; exit 1; }
  case $d in Windows/*) dst=$D/${d#Windows/};; *) dst=$OUT/drive_c/$d;; esac
  mkdir -p "$dst"; cp -r "$IMG/$d/." "$dst/"
done

# The games' registrations in the Games folder (GameUX\Games: title, program,
# ratings), from which the Start menu's Games links and the most-used list's
# Solitaire are made: as Windows setup writes them, but with the image's
# system drive (D: while it was serviced offline) as the prefix's, C:.
export REGISTRY_LIB=${REGISTRY_LIB:-$HOME/gex-sandbox/scratch/X/pylib}
gk='Microsoft\Windows\CurrentVersion\GameUX\Games'
python3 -I "$T/hive-reg.py" "$OUT" "$S/config/SOFTWARE:$gk=Software\\$gk!"
python3 -I - "$OUT/system.reg" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p, encoding='utf-8').read().split('\n'); inside = False
for i, l in enumerate(s):
    if l.startswith('['): inside = l.lower().startswith('[software\\\\microsoft\\\\windows\\\\currentversion\\\\gameux\\\\games')
    elif inside: s[i] = re.sub(r'"D:(?=\\\\)', lambda m: '"C:', l, flags=re.I)
open(p, 'w', encoding='utf-8').write('\n'.join(s))
PY

# the Start menu's shortcuts, all users' (ProgramData) and the default
# user's, which the Start menu lists under All Programs and from which it
# seeds a new user's most-used list; Wine's (empty) folders out of the way
sm='Microsoft/Windows/Start Menu'
if [ -d "$IMG/ProgramData/$sm" ]; then
  chmod -R u+w "$OUT/drive_c/ProgramData/$sm" 2>/dev/null || true
  rm -rf "$OUT/drive_c/ProgramData/$sm"; cp -r "$IMG/ProgramData/$sm" "$OUT/drive_c/ProgramData/${sm%/*}/"
  # A folder's desktop.ini is hidden and system on Windows, so the shell
  # never lists it as an item; gex keeps no DOS attributes (Wine stores them
  # in an extended attribute, which gex's file systems lack), and the shell
  # would start the Startup folder's as a program. That folder's names no
  # item, so it goes.
  rm -f "$OUT/drive_c/ProgramData/$sm/Programs/Startup/desktop.ini"
  for u in "$OUT"/drive_c/users/*/AppData/Roaming; do
    [ -d "$u/$sm" ] && [ ! -L "$u" ] || continue
    chmod -R u+w "$u/$sm" 2>/dev/null || true
    rm -rf "$u/$sm"; cp -r "$IMG/Users/Default/AppData/Roaming/$sm" "$u/${sm%/*}/"
    mkdir -p "$u/$sm/Programs/Startup"
  done
  # The shortcuts Windows setup makes from its components' manifests (Sticky
  # Notes, Snipping Tool, Media Center...), which an extracted image lacks,
  # and Internet Explorer's, which Internet Explorer's per-user setup
  # (ie4uinit) puts in each user's own Programs folder (the image's own
  # Administrator profile has it there, with the program on another drive):
  # a new user's taskbar pins and most-used list are seeded from these, the
  # Internet Explorer and Windows Explorer pins and Magnify from the user's
  # Programs folder (FOLDERID_Programs), the rest from all users'.
  python3 -I "$T/manifest-lnk.py" "$IMG/Windows/winsxs/Manifests" "$OUT/drive_c" | sed 's/^/stage: made /'
  for u in "$OUT"/drive_c/users/*/AppData/Roaming; do
    [ -d "$u/$sm" ] && [ ! -L "$u" ] || continue
    python3 -I "$T/manifest-lnk.py" --link "$OUT/drive_c" "${u#$OUT/drive_c/}/$sm/Programs" 'Internet Explorer.lnk' \
      '%ProgramFiles%\Internet Explorer\iexplore.exe' '@"%windir%\System32\ie4uinit.exe",-732' \
      '@%SystemRoot%\system32\shell32.dll,-21781' | sed 's/^/stage: made /'
  done
  # Only shortcuts to programs the prefix has: the Start menu seeds its
  # most-used list from these, and one whose program is not here shows a
  # blank icon and starts nothing.
  python3 -I "$T/lnk-target.py" --prune "$OUT/drive_c" "$OUT/drive_c/ProgramData/$sm" \
    "$OUT"/drive_c/users/*/AppData/Roaming/"$sm" | sed 's/^/stage: /'
fi

# A folder whose desktop.ini names its items (LocalizedFileNames: "Getting
# Started" for Welcome Center.lnk, "Magnifier" for Magnify.lnk) is read only
# or system on Windows; the shell reads the file only then. Wine has no DOS
# attributes without extended attributes (gex's file systems have none) and
# reports a directory without write permission read only, so the Start
# menus' folders that have one lose theirs (chmod -R u+w before removing a
# prefix staged so).
find "$OUT/drive_c/ProgramData/$sm" "$OUT"/drive_c/users/*/AppData/Roaming/"$sm" -type f -iname desktop.ini \
  -printf '%h\0' 2>/dev/null | xargs -0 -r chmod a-w

