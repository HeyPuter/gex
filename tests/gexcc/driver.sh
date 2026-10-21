#!/bin/sh
# gexcc's driver on what configure scripts and Debian sources hand it, with
# nothing run: a source on stdin, a link to /dev/null (pass and fail), and
# top-level `.symver` asm (left out, the symbol still defined, the
# dependency file still naming the object).
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
exit $fail
