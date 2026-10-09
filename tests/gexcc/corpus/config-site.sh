#!/bin/sh
# Regenerate tools/gexcc/config.site: the cache variables whose answers a
# native-mode configure (its checks run under gexc) and a cross-mode one
# (guesses) disagree on, with the native answers.
#   config-site.sh NATIVE_TREES_DIR CROSS_TREES_DIR > config.site
# Each dir holds configured trees (*/config.log). Running configure natively
# starts one host gexc per check: do it one tree at a time, inside
#   systemd-run --user --scope -p TasksMax=256 nice -n 19
set -eu
ext() { awk '/^## Cache variables. ##/{f=1;next} f && /^[A-Za-z_0-9]+=/ {print; seen=1; next} f && seen && /^## /{exit}' "$@"; }
t=$(mktemp -d)
ext "$1"/*/config.log | sort -t= -k1,1 -u > "$t/n"
ext "$2"/*/config.log | sort -t= -k1,1 -u > "$t/c"
sed -n '1,9p' "$(dirname "$0")/../../../tools/gexcc/config.site"
join -t= "$t/n" "$t/c" | awk -F= '$2 != $3' |
  grep -v '^ac_cv_build\|^ac_cv_host\|^ac_cv_env\|^ac_cv_path\|^ac_cv_prog\|^ac_cv_objext\|^am_cv\|^ac_cv_exeext\|^gl_cv_compiler_check_decl_option' |
  while IFS= read -r l; do k=${l%%=*}; v=$(grep "^$k=" "$t/n" | head -1); echo "$k=\${$k=${v#*=}}"; done
rm -rf "$t"
