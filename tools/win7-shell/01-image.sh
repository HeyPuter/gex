#!/bin/bash
# 01: the Windows 7 image, from the user's ISO, into $W7_OUT/img (the root
# holding Windows/, Program Files/, ...). Nothing from it is executed here,
# and nothing from it goes into git: every later step copies what it needs.
# Needs 7-Zip's 7zz (Debian/Ubuntu: apt install 7zip; or 7-Zip's own
# linux-x64 release), which reads the ISO (UDF) and the WIM. The x86 SP1
# ISO's install.wim holds one image (INDEX, default 1).
#   01-image.sh ISO
. "$(dirname "$0")/lib.sh"
ISO=$(realpath "${1:?usage: 01-image.sh ISO}"); SEVENZIP=${SEVENZIP:-7zz}; IMG=$W7_OUT/img
command -v "$SEVENZIP" > /dev/null || { echo "no $SEVENZIP (apt install 7zip)" >&2; exit 2; }
step "install.wim from $ISO"
rm -rf "$W7_OUT/iso" "$IMG"; mkdir -p "$W7_OUT/iso"
"$SEVENZIP" e -y -o"$W7_OUT/iso" "$ISO" sources/install.wim > "$W7_OUT/logs/image.log" 2>&1 || { tail "$W7_OUT/logs/image.log"; exit 1; }
n=$("$SEVENZIP" l "$W7_OUT/iso/install.wim" | sed -n 's/^Images = //p')
step "the image (${INDEX:-1} of ${n:-1})"
src=$W7_OUT/iso/install.wim; [ "${n:-1}" -le 1 ] || src="$src ${INDEX:-1}/*"
"$SEVENZIP" x -y -snl -o"$IMG" $src >> "$W7_OUT/logs/image.log" 2>&1 || { tail "$W7_OUT/logs/image.log"; exit 1; }
[ -f "$IMG/Windows/System32/config/SOFTWARE" ] || { echo "no Windows/System32/config/SOFTWARE in the image" >&2; exit 1; }
rm -rf "$W7_OUT/iso"
step "done: $IMG ($(du -sh "$IMG" | cut -f1))"
