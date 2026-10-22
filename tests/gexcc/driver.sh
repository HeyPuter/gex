#!/bin/sh
# gexcc's driver on what configure scripts and Debian sources hand it, with
# nothing run: a source on stdin, a link to /dev/null (pass and fail), and
# top-level `.symver` asm (left out, the symbol still defined, the
# dependency file still naming the object), and a program's container
# carrying section headers (Meson's rpath fixer reads them at install).
#
#   tests/gexcc/driver.sh        (gexcc on PATH: tools/gexcc/install.sh)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (tools/gexcc/install.sh)"; exit 2; }
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
fail=0
check() { if [ "$1" = 0 ]; then echo "ok $2"; else echo "FAIL $2"; fail=1; fi; }

printf '#include <stdlib.h>\nint main(void){return reallocarray(NULL,0,0)==NULL;}\n' \
    | gexcc -Werror=implicit-function-declaration -x c -o /dev/null - > "$W/log" 2>&1
check $? "a source on stdin, linked to /dev/null"
printf 'int no_such_function(void);\nint main(void){return no_such_function();}\n' \
    | gexcc -x c -o /dev/null - > "$W/log" 2>&1
[ $? != 0 ]; check $? "a link to /dev/null with an undefined symbol fails"
[ ! -e /dev/null ] || [ -c /dev/null ]; check $? "/dev/null is still a character device"

cat > "$W/sv.c" <<'EOF'
int libx_get(int a) { return a + 1; }
#define SYMVER(cn, vn) __asm__(".symver " #cn "," vn)
SYMVER(libx_get, "get@X_1.0");
__asm__("\t.symver libx_get,get2@@X_2.0");
EOF
gexcc -c "$W/sv.c" -o "$W/sv.o" -MD -MF "$W/sv.d" 2> "$W/log"
check $? ".symver asm compiles"
grep -q "2 .symver directive(s) left out" "$W/log"; check $? "and says how many it left out"
grep -q "^$W/sv.o:" "$W/sv.d"; check $? "the dependency file names the object"
llvm-nm "$W/sv.o" | grep -q " T libx_get$"; check $? "the versioned symbol is still defined"
# as TRIPLE-cpp: stdin with no file, `cpp IN OUT`
ln -s "$(command -v gexcc)" "$W/wasm32-linux-musl-cpp"
[ "$(printf 'l unix if\n' | "$W/wasm32-linux-musl-cpp" -undef 2>/dev/null | tail -1)" = "l unix if" ]
check $? "cpp -undef reads standard input"
printf '#define A 7\nA\n' > "$W/a.h"; "$W/wasm32-linux-musl-cpp" "$W/a.h" "$W/a.out" 2>/dev/null
[ "$(tail -1 "$W/a.out")" = 7 ]; check $? "cpp IN OUT"
printf 'int main(void){return 0;}\n' | gexcc -x c -o "$W/prog" - 2> "$W/log"
[ "$(readelf -h "$W/prog" | awk '/Section header string table index/{print $NF}')" = 1 ] && \
    readelf -S "$W/prog" | grep -q '\.shstrtab'
check $? "a program has a section names' table"
# the default version's name is an alias of the C name (fuse_parse_cmdline
# is fuse_parse_cmdline_312), an older version's is left out
cat > "$W/sv2.c" <<'EOF2'
int lib_get_312(int a) { return a + 2; }
int lib_get_30(int a) { return a + 1; }
__asm__(".symver lib_get_312,lib_get@@L_3.12");
__asm__(".symver lib_get_30,lib_get@L_3.0");
EOF2
gexcc -c "$W/sv2.c" -o "$W/sv2.o" 2> "$W/log" && llvm-nm "$W/sv2.o" | grep -q " T lib_get$"
check $? "a default-version .symver name is an alias"
# a shared library may leave a symbol to the program (libfl's main, yylex)
printf 'int yylex(void);\nint main(void){return yylex();}\n' > "$W/fl.c"
GEXCC_SHARED=1 gexcc -shared -fPIC -o "$W/libfl.so" "$W/fl.c" 2> "$W/log"
check $? "a shared library links with an undefined symbol"
GEXCC_SHARED=1 gexcc -shared -fPIC -Wl,-z,defs -o "$W/libfl2.so" "$W/fl.c" 2> "$W/log"
[ $? != 0 ]; check $? "but not with -z defs"
# glibc-isms: pread64 after _GNU_SOURCE, a size_t * for a uint32_t *, C++ register
printf '#define _GNU_SOURCE\n#include <unistd.h>\n#include <stdint.h>\n#include <stddef.h>\n'\
'static int f(uint32_t *p){return *p;}\nint g(int fd, char *b, size_t n){off64_t o=0; return pread64(fd,b,n,o)+f(&n);}\n' > "$W/g.c"
gexcc -c -o "$W/g.o" "$W/g.c" 2> "$W/log"
check $? "pread64 under _GNU_SOURCE, size_t * as uint32_t *"
printf 'int h(int a){register int b = a; return b;}\n' > "$W/r.cpp"
gexc++ -std=c++17 -c -o "$W/r.o" "$W/r.cpp" 2> "$W/log"
check $? "register in C++17"
printf 'static inline int unused(int a){return a;}\nint used(void){return 1;}\n' > "$W/u.c"
gexcc -Wall -Werror -c -o "$W/u.o" "$W/u.c" 2> "$W/log"
check $? "an unused static inline function under -Werror"
# -v on a link is verbose (CMake's compiler ABI check), not a query
printf 'int main(void){return 0;}\n' > "$W/v.c"
gexcc -c -o "$W/v.o" "$W/v.c" && gexcc -v -Wl,-v "$W/v.o" -o "$W/v" 2> "$W/log" && [ -s "$W/v" ]
check $? "-v -Wl,-v on a link still links"
# -Wl,--version-script FILE (kmod): the file is the option's, not an input
printf 'LIB_1 { global: used; local: *; };\n' > "$W/lib.sym"
GEXCC_SHARED=1 gexcc -shared -fPIC -Wl,--version-script "$W/lib.sym" -o "$W/libu.so" "$W/u.c" 2> "$W/log"
check $? "-Wl,--version-script FILE with the file as its own argument"
# a target library's -I/usr/include/DIR (tclConfig.sh) is the sysroot's
# when the build machine has no such directory
SR=$(gexcc -print-sysroot)
if [ ! -e /usr/include/gexcc-driver-test ] && [ -w "$SR/usr/include" ]; then
    mkdir -p "$SR/usr/include/gexcc-driver-test"
    echo '#define DRIVER_TEST 3' > "$SR/usr/include/gexcc-driver-test/dt.h"
    printf '#include <dt.h>\nint t(void){return DRIVER_TEST;}\n' > "$W/dt.c"
    gexcc -I/usr/include/gexcc-driver-test -c -o "$W/dt.o" "$W/dt.c" 2> "$W/log"
    r=$?
    rm -r "$SR/usr/include/gexcc-driver-test"
    check $r "-I/usr/include/DIR is found in the sysroot"
fi
# glibc's renameat2 flags in <stdio.h> (fuse3), with <linux/fs.h> either side
printf '#define _GNU_SOURCE\n#include <stdio.h>\n#include <linux/fs.h>\nint f(void){return RENAME_EXCHANGE;}\n' > "$W/rn.c"
printf '#define _GNU_SOURCE\n#include <linux/fs.h>\n#include <stdio.h>\nint f(void){return RENAME_NOREPLACE;}\n' > "$W/rn2.c"
gexcc -Werror -c -o "$W/rn.o" "$W/rn.c" 2> "$W/log" && gexcc -Werror -c -o "$W/rn2.o" "$W/rn2.c" 2>> "$W/log"
check $? "RENAME_EXCHANGE from <stdio.h> under _GNU_SOURCE"
# two objects with one file name (libblkid's partitions/minix.c and
# superblocks/minix.c) are two inputs, not one side module twice
mkdir -p "$W/da" "$W/db"
printf 'int fa(void){return 1;}\n' > "$W/da/m.c"; printf 'int fb(void){return 2;}\n' > "$W/db/m.c"
GEXCC_SHARED=1 gexcc -fPIC -c "$W/da/m.c" -o "$W/da/la-m.o" && GEXCC_SHARED=1 gexcc -fPIC -c "$W/db/m.c" -o "$W/db/la-m.o" &&
    GEXCC_SHARED=1 gexcc -shared -o "$W/libd.so" "$W/da/la-m.o" "$W/db/la-m.o" 2> "$W/log"
check $? "a shared library of two objects with the same file name"
# -pthread defines _REENTRANT (AX_PTHREAD), compiling and linking at once
printf '#ifndef _REENTRANT\n#error no _REENTRANT\n#endif\nint main(void){return 0;}\n' > "$W/re.c"
gexcc -pthread -c -o "$W/re.o" "$W/re.c" 2> "$W/log" && gexcc -pthread -o "$W/re" "$W/re.c" 2>> "$W/log"
check $? "-pthread defines _REENTRANT"
# an empty shared link (libbsd's output-format probe), and a GNU ld script
# in a library's place (libbsd.so: GROUP(libbsd.so.0 AS_NEEDED(...)))
GEXCC_SHARED=1 gexcc -shared -nostdlib -nostartfiles -x assembler /dev/null -o "$W/empty.so" 2> "$W/log"
check $? "an empty shared link"
mkdir -p "$W/ls"
printf 'int ls_get(void){return 7;}\n' > "$W/ls/l.c"
GEXCC_SHARED=1 gexcc -shared -fPIC -Wl,-soname,libls.so.0 -o "$W/ls/libls.so.0" "$W/ls/l.c" &&
    printf '/* GNU ld script */\nOUTPUT_FORMAT(elf32-i386)\nGROUP(%s/ls/libls.so.0 AS_NEEDED(-lm))\n' "$W" > "$W/ls/libls.so" &&
    printf 'int ls_get(void);\nint main(void){return ls_get() != 7;}\n' > "$W/ls/m.c" &&
    GEXCC_SHARED=1 gexcc -o "$W/ls/m" "$W/ls/m.c" -L"$W/ls" -lls 2> "$W/log"
check $? "a GNU ld script in a library's place"
# Debian's -Werror=implicit-function-declaration does not undo the warning
# for a C library function (strcmp after <stdlib.h> alone, as glibc allows)
printf '#include <stdlib.h>\nint main(void){return strcmp("a","b");}\n' > "$W/id.c"
gexcc -O2 -Werror=implicit-function-declaration -c -o "$W/id.o" "$W/id.c" 2> "$W/log"
check $? "an undeclared C library call under Debian's -Werror=implicit-function-declaration"
# a program's variable with a C library function's name (hunspell's `int
# warn`) links, the program's code reaching its own, as on ELF
printf 'int warn = 5;\nint get(void);\nint main(void){return get() != 5;}\n' > "$W/wv.c"
printf 'extern int warn;\nint get(void){return warn;}\n' > "$W/wg.c"
GEXCC_SHARED=1 gexcc -c -o "$W/wv.o" "$W/wv.c" && GEXCC_SHARED=1 gexcc -c -o "$W/wg.o" "$W/wg.c" &&
    GEXCC_SHARED=1 gexcc -o "$W/wv" "$W/wv.o" "$W/wg.o" 2> "$W/log"
check $? "a program's variable named as a C library function"
# Debian's build flags for GCC: spec files, LTO, `-z relro`
printf 'int main(void){return 0;}\n' > "$W/df.c"
gexcc -Werror -flto=auto -ffat-lto-objects -specs=/usr/share/dpkg/pie-compile.specs -c -o "$W/df.o" "$W/df.c" 2> "$W/log" &&
    gexcc -flto=auto -specs=/usr/share/dpkg/pie-link.specs -z relro -z now -o "$W/df" "$W/df.o" 2>> "$W/log"
check $? "Debian's GCC flags (-specs=, -flto=auto -ffat-lto-objects, -z relro)"
# a GNU ld link warning (alsa-lib's link_warning) is left out
cat > "$W/lw.c" <<'EOF2'
#define link_warning(symbol, msg) \
  asm (".section " ".gnu.warning." #symbol "\n\t.previous"); \
  static const char __evoke_link_warning_##symbol[] \
    __attribute__ ((section (".gnu.warning." #symbol "\n\t#"))) = msg;
int old_api(void) { return 1; }
link_warning(old_api, "old_api is deprecated")
EOF2
gexcc -c -o "$W/lw.o" "$W/lw.c" 2> "$W/log" && llvm-nm "$W/lw.o" | grep -q " T old_api$"
check $? "a GNU ld link warning is left out"
# GNU gettext's configure test for the C library's gettext links (NLS on)
printf '#include <libintl.h>\nextern int _nl_msg_cat_cntr;\nextern const char *_nl_expand_alias(const char *);\n'\
'int main(void){bindtextdomain("",""); return *gettext("") + _nl_msg_cat_cntr + *_nl_expand_alias("");}\n' > "$W/nl.c"
gexcc -o "$W/nl" "$W/nl.c" 2> "$W/log" && GEXCC_SHARED=1 gexcc -o "$W/nl2" "$W/nl.c" 2>> "$W/log"
check $? "gettext.m4's test for GNU gettext in the C library links"
# -I/usr/include (cyrus-sasl's configure) names the target's system headers,
# not the build machine's glibc ones
printf '#include <stdlib.h>\nint main(void){return 0;}\n' > "$W/ui.c"
gexcc -I/usr/include -I /usr/include/ -isystem/usr/include -c -o "$W/ui.o" "$W/ui.c" 2> "$W/log"
check $? "-I/usr/include does not reach the build machine's headers"
# a library function returning int called through a void declaration
# (gettext's tputs): the call goes through, its result dropped
mkdir -p "$W/vr"
printf 'int vr_hits;\nint vr(int n){vr_hits += n; return 9;}\n' > "$W/vr/l.c"
printf 'extern int vr_hits;\nvoid vr(int);\nint main(void){vr(3); vr(4); return vr_hits != 7;}\n' > "$W/vr/m.c"
GEXCC_SHARED=1 gexcc -shared -fPIC -o "$W/vr/libvr.so" "$W/vr/l.c" &&
    GEXCC_SHARED=1 gexcc -o "$W/vr/m" "$W/vr/m.c" -L"$W/vr" -lvr 2> "$W/log" &&
    grep -q 'its result is dropped' "$W/log" && ! grep -q 'calls to it trap' "$W/log"
check $? "a void declaration of a library's int function calls it"
exit $fail
