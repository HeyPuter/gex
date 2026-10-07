# Corpus recipes (run.sh). build_NAME runs in the unpacked tree with `set -e`;
# test_NAME prints `RESULT pass=N fail=N skip=N`. Libraries other programs
# use install into the sysroot (--prefix=/usr, DESTDIR=$SYSROOT).
#
# Test suites run INSIDE one gexc (guest), never as one host gexc per test
# program (2026-10-07: thousands of host gexc took gooner down):
#   - configure in cross mode (--build given): no test programs run on the host;
#   - the host builds the suite's programs without running them;
#   - `guest CMD` runs CMD under one gexc: busybox's sh and tools, the gexcc
#     GNU make in $GUEST_BIN; every test process is a guest process.

# suites safe to run: in the guest, or a handful of host gexc at most (lua,
# sqlite, dash, less: one or two). The rest still start a host gexc per test
# and are refused unless HOSTRUN=1 (then one at a time, in a resource scope).
GUEST_SUITES="zlib xz bzip2 jq pcre2 expat libpng coreutils grep sed diffutils findutils libevent lua sqlite dash less gawk tar"

# autotools, cross mode: configure runs nothing it builds
# (config.site: what the run-time checks found on gex itself)
at_configure() { CONFIG_SITE=$GEXCC_PREFIX/share/gexcc/config.site ./configure --host=$T --build=x86_64-pc-linux-gnu --prefix=/usr "$@"; }
# a shell command line in the guest, in this directory
# With COUNT=1, `GUEST host-execs N`: host programs the guest run started (execve in the
# gexc's process tree, counted by strace; gexc itself is 1), `GUEST
# host-clones N`: threads and processes that tree made (clone, clone3), and
# `GUEST machine-forks N`: processes created on the whole machine meanwhile
# (/proc/stat; includes other users').
guest() {
  local p0 p1 r out
  out=$(mktemp)
  p0=$(awk '/^processes/{print $2}' /proc/stat)
  # (strace only with COUNT=1: it slows a long suite down)
  ${COUNT:+strace -f -qq --seccomp-bpf -e trace=execve,clone,clone3,fork,vfork -e signal=none -c -o "$out"} \
    timeout ${TMO:-3600} gexcc-exec run --host-env --env PATH=$GUEST_BIN:/bin:/usr/bin --env SHELL=/bin/sh \
    --cwd "$PWD" /bin/sh -c "$*"
  r=$?
  p1=$(awk '/^processes/{print $2}' /proc/stat)
  [ -n "${COUNT:-}" ] && echo "GUEST host-execs $(awk '$NF == "execve" {print $4}' "$out")" >&2
  [ -n "${COUNT:-}" ] && echo "GUEST host-clones $(awk '$NF ~ /^(clone|clone3|fork|vfork)$/ {n += $4} END {print n + 0}' "$out")" >&2
  echo "GUEST machine-forks $((p1 - p0))" >&2
  rm -f "$out"
  return $r
}

# automake's test-suite summary (# PASS: n ...) summed over every log
am_result() {
  awk '/^# PASS:/{p+=$3} /^# FAIL:/{f+=$3} /^# ERROR:/{f+=$3} /^# XPASS:/{f+=$3} /^# SKIP:/{s+=$3} /^# XFAIL:/{x+=$3}
       END{printf "RESULT pass=%d fail=%d skip=%d\n", p+x, f, s}' "$@"
}
# automake: the host builds the suite (TESTS empty runs nothing), the guest runs it
am_check() {
  make -j$JOBS check TESTS= XFAIL_TESTS= > check-build.out 2>&1 || { tail -20 check-build.out; echo "RESULT pass=0 fail=1 skip=0"; return; }
  guest make -k check SHELL=/bin/sh "$@" > check.out 2>&1; cat check.out; am_result check.out
}

build_zlib() {
  CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib CHOST=$T ./configure --prefix=/usr
  make -j$JOBS
  make install DESTDIR=$SYSROOT
}
test_zlib() {
  guest make test SHELL=/bin/sh 2>&1 | tee t.out
  n=$(grep -c 'test OK' t.out); echo "RESULT pass=$n fail=$((3 - n)) skip=0"
}

build_xz() { at_configure --disable-nls; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_xz() { am_check; }

# plain Makefile
build_bzip2() {
  make -j$JOBS CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib bzip2 bzip2recover
}
test_bzip2() {
  guest make check SHELL=/bin/sh CC=true 2>&1 | tee t.out
  # six round trips (three compress, three decompress), each a cmp
  f=$(grep -c 'differ\|EOF on\|Error\|rror' t.out)
  echo "RESULT pass=$((6 - f)) fail=$f skip=0"
}

build_lua() {
  make -j$JOBS posix CC=gexcc AR="gexcc-ar rcu" RANLIB=gexcc-ranlib MYCFLAGS=-DLUA_USE_POSIX
}
test_lua() {
  rm -rf lt && mkdir lt && tar xf "$GEXCC_SRC/lua-5.4.7-tests.tar.gz" -C lt --strip-components=1
  cd lt
  # musl's setlocale accepts any name (as C.UTF-8), so the suite's
  # "pt_BR exists" probe is wrong there: skip the decimal-comma checks
  sed -i 's/^if os.setlocale("pt_BR") or os.setlocale("ptb") then/if false then/' literals.lua
  # the portable suite (_port: no OS-specific checks); one RESULT for all.lua
  if timeout ${TMO:-3600} ../src/lua -e"_U=true _port=true" all.lua > all.out 2>&1; then
    tail -5 all.out; echo "RESULT pass=1 fail=0 skip=0"
  else
    tail -20 all.out; echo "RESULT pass=0 fail=1 skip=0"
  fi
}

build_sqlite() { at_configure --disable-shared; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_sqlite() {
  # the autoconf tarball has no test suite: a scripted session against the
  # host's sqlite3 when present
  cat > t.sql <<'SQL'
create table t(a integer primary key, b text, c real);
insert into t(b,c) values ('x',1.5),('y',2.5),('z',-3);
with recursive n(i) as (select 1 union all select i+1 from n where i<1000) insert into t(b,c) select 'r'||i, i*0.5 from n;
select count(*), sum(c), group_concat(b) filter (where a<4) from t;
create index ti on t(b); select a from t where b='r500';
select printf('%.3f', avg(c)), max(length(b)) from t;
select json_object('a',1,'b',json_array(1,2,3));
pragma integrity_check;
SQL
  ./sqlite3 db.gex < t.sql > gex.out 2>&1; rm -f db.gex
  if command -v sqlite3 >/dev/null; then sqlite3 db.host < t.sql > host.out 2>&1; rm -f db.host; else cp gex.out host.out; fi
  cat gex.out
  if cmp -s gex.out host.out; then echo "RESULT pass=1 fail=0 skip=0"; else diff host.out gex.out; echo "RESULT pass=0 fail=1 skip=0"; fi
}

build_jq() { at_configure --with-oniguruma=builtin --disable-docs --disable-maintainer-mode; make -j$JOBS; }
# jq's src/version.h depends on .remake-version-h, which only exists after
# the version changed, so every make regenerates it and recompiles main.c;
# the guest has no compiler, so the marker is made, older than version.h
test_jq() {
  make -j$JOBS check TESTS= > check-build.out 2>&1 || { tail -20 check-build.out; echo "RESULT pass=0 fail=1 skip=0"; return; }
  touch -d 2000-01-01 .remake-version-h
  guest make -k check SHELL=/bin/sh > check.out 2>&1; cat check.out; am_result check.out
}

build_pcre2() { at_configure --enable-pcre2-16 --enable-pcre2-32; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_pcre2() { am_check; }

build_expat() { at_configure --without-docbook --without-examples; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_expat() { am_check; }

build_libpng() { at_configure; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_libpng() { am_check; }

# --- core userland (GNU: automake test suites) ---
build_coreutils() { FORCE_UNSAFE_CONFIGURE=1 at_configure --disable-nls --without-selinux --without-libgmp; make -j$JOBS; }
test_coreutils() { am_check; }
build_grep() { at_configure --disable-nls; make -j$JOBS; }
test_grep() { am_check; }
build_sed() { at_configure --disable-nls; make -j$JOBS; }
test_sed() { am_check; }
build_gawk() { at_configure --disable-nls --without-readline; make -j$JOBS; }
test_gawk() {
  # gawk's own runner: "ALL TESTS PASSED" or a list of failures
  guest make -k check SHELL=/bin/sh > check.out 2>&1; cat check.out
  if grep -q 'ALL TESTS PASSED' check.out; then
    n=$(ls test/*.ok 2>/dev/null | wc -l); echo "RESULT pass=$n fail=0 skip=0"
  else
    f=$(sed -n 's/^\([0-9][0-9]*\) TESTS* FAILED.*/\1/p' check.out | tail -1)
    n=$(ls test/*.ok 2>/dev/null | wc -l); echo "RESULT pass=$((n - ${f:-0})) fail=${f:-0} skip=0"
  fi
}
build_diffutils() { at_configure --disable-nls; make -j$JOBS; }
test_diffutils() { am_check; }
build_findutils() { at_configure --disable-nls; make -j$JOBS; }
test_findutils() { am_check; }
build_tar() { FORCE_UNSAFE_CONFIGURE=1 at_configure --disable-nls; make -j$JOBS; }
test_tar() {
  # autotest: "N tests were successful" / "ERROR: N tests were run, M failed"
  make -j$JOBS check TESTSUITEFLAGS=--version > check-build.out 2>&1
  guest make -k check SHELL=/bin/sh TESTSUITEFLAGS=-j$JOBS > check.out 2>&1; tail -30 check.out
  ok=$(grep -o '^[0-9]* tests* were successful' check.out | awk '{print $1}')
  if [ -n "$ok" ]; then sk=$(grep -o '[0-9]* tests* were skipped' check.out | awk '{print $1}'); echo "RESULT pass=$ok fail=0 skip=${sk:-0}"
  else
    l=$(grep -E '^ERROR: [0-9]+ tests* were run' check.out | tail -1)
    run=$(echo "$l" | awk '{print $2}'); f=$(echo "$l" | sed -n 's/.*run, *\([0-9]*\) failed.*/\1/p')
    sk=$(grep -o '[0-9]* tests* were skipped' check.out | awk '{print $1}')
    echo "RESULT pass=$((run - ${f:-0})) fail=${f:-0} skip=${sk:-0}"
  fi
}
# GNU make for the guest (the in-guest test runs need it): $GUEST_BIN/make
build_make() { at_configure --disable-nls; make -j$JOBS; mkdir -p "$GUEST_BIN"; cp make "$GUEST_BIN/make"; }
test_make_host() {
  timeout ${TMO:-3600} make -k check > check.out 2>&1; tail -20 check.out
  l=$(grep -E '[0-9]+ Tests? in [0-9]+ Categor' check.out | tail -1)
  # "N Tests in M Categories Complete ... No Failures" or "x Tests Failed"
  t=$(echo "$l" | awk '{print $1}')
  f=$(grep -oE '[0-9]+ Tests? Failed' check.out | tail -1 | awk '{print $1}')
  echo "RESULT pass=$(( ${t:-0} - ${f:-0} )) fail=${f:-0} skip=0"
}
build_bash() { export GEXCC_FPCAST=1; at_configure --without-bash-malloc --disable-nls --with-curses CFLAGS="-O2 -Wno-implicit-function-declaration"; make -j$JOBS; }
test_bash() {
  # bash's suite prints diffs; count the run-* scripts that print nothing
  cd tests
  pass=0; fail=0
  for t in run-*; do
    [ "$t" = run-all ] && continue
    [ "$t" = run-minimal ] && continue
    o=$(THIS_SH=$PWD/../bash PATH=$PWD:$PATH BUILD_DIR=$PWD/.. BASH_TSTOUT=$PWD/../tstout.$t \
        timeout 300 $PWD/../bash ./$t 2>&1)
    if [ -z "$(printf '%s' "$o" | grep -v '^warning:')" ]; then pass=$((pass + 1)); else fail=$((fail + 1)); echo "== $t"; printf '%s\n' "$o" | head -20; fi
  done
  echo "RESULT pass=$pass fail=$fail skip=0"
}
build_dash() { at_configure; make -j$JOBS; }
test_dash() {
  # no suite of its own: a script against the host's dash/sh
  cat > t.sh <<'SH'
set -e
x=$(echo a b | tr ' ' '\n' | sort -r | head -1); echo "$x"
for i in 1 2 3; do echo $((i * i)); done
f() { local v=$1; echo "f:$v"; return 3; }; f q || echo "rc $?"
case foo.c in *.c) echo C;; *) echo other;; esac
( exit 4 ) || echo "sub $?"
trap 'echo trapped' EXIT
echo ${HOME:+set} ${UNSET_VAR:-dflt} ${#x}
cat <<EOF2 | wc -l
1
2
EOF2
read a b <<EOF3
one two
EOF3
echo "$b $a"
printf '%05d|%-3s|%x\n' 42 ab 255
kill -0 $$ && echo alive
wait
SH
  ./src/dash t.sh > gex.out 2>&1; s1=$?
  sh t.sh > host.out 2>&1; s2=$?
  cat gex.out
  if cmp -s gex.out host.out && [ $s1 = $s2 ]; then echo "RESULT pass=1 fail=0 skip=0"; else diff host.out gex.out; echo "RESULT pass=0 fail=1 skip=0"; fi
}
build_ncurses() {
  at_configure --without-cxx-binding --without-ada --without-manpages --without-tests --enable-widec \
    --with-termlib --disable-stripping --without-shared --with-fallbacks=xterm,vt100,linux,screen,xterm-256color \
    --with-default-terminfo-dir=/usr/share/terminfo --enable-pc-files --with-pkg-config-libdir=/usr/lib/pkgconfig
  make -j$JOBS
  make install DESTDIR=$SYSROOT
  # the names configure scripts probe for (-ltinfo, -lncurses, -lcurses)
  for l in tinfo ncurses curses; do ln -sf lib${l}w.a $SYSROOT/usr/lib/lib$l.a 2>/dev/null || true; done
  ln -sf libncursesw.a $SYSROOT/usr/lib/libcurses.a
  [ -e $SYSROOT/usr/include/curses.h ] || ln -sf ncursesw/curses.h $SYSROOT/usr/include/curses.h
  [ -e $SYSROOT/usr/include/term.h ] || ln -sf ncursesw/term.h $SYSROOT/usr/include/term.h
  [ -e $SYSROOT/usr/include/ncurses.h ] || ln -sf ncursesw/ncurses.h $SYSROOT/usr/include/ncurses.h
}
build_less() {
  CPPFLAGS="-I$SYSROOT/usr/include/ncursesw" at_configure --with-regex=posix
  make -j$JOBS
}
test_less() {
  # non-interactive: less as a filter (not a terminal) copies its input
  seq 1 2000 > in.txt
  ./less -f in.txt | cmp - in.txt && ./less -V | head -1 && echo "RESULT pass=1 fail=0 skip=0" || echo "RESULT pass=0 fail=1 skip=0"
}

# --- bigger programs ---
build_mbedtls() {
  cmake -S . -B b -G Ninja -DCMAKE_TOOLCHAIN_FILE=$GEXCC_PREFIX/share/gexcc/toolchain.cmake \
    -DCMAKE_BUILD_TYPE=Release -DENABLE_TESTING=ON -DENABLE_PROGRAMS=ON -DMBEDTLS_FATAL_WARNINGS=OFF
  ninja -C b -j$JOBS
  cmake --install b
}
test_mbedtls() {
  (cd b && timeout ${TMO:-3600} ctest -j$JOBS --output-on-failure) > ct.out 2>&1; tail -30 ct.out
  ctest_result ct.out
}
# ctest's "N% tests passed, F tests failed out of T"
ctest_result() {
  l=$(grep -E 'tests passed, [0-9]+ tests failed out of' "$1" | tail -1)
  f=$(echo "$l" | sed -n 's/.* \([0-9]*\) tests failed out of \([0-9]*\).*/\1/p')
  t=$(echo "$l" | sed -n 's/.* tests failed out of \([0-9]*\).*/\1/p')
  echo "RESULT pass=$(( ${t:-0} - ${f:-0} )) fail=${f:-0} skip=0"
}
build_curl() {
  at_configure --with-mbedtls=$SYSROOT/usr --without-libpsl --disable-ldap --without-brotli --without-zstd \
    --without-libidn2 --without-nghttp2 --with-zlib=$SYSROOT/usr --disable-shared
  make -j$JOBS
}
test_curl() {
  # the suite's runner is perl (the host's): test programs and curl run under gexc
  timeout ${TMO:-5400} make test-full TFLAGS="-j$JOBS -a -p" > t.out 2>&1
  tail -15 t.out
  l=$(grep -E '^TESTDONE: [0-9]+ tests out of [0-9]+ reported OK' t.out | tail -1)
  ok=$(echo "$l" | awk '{print $2}'); tot=$(echo "$l" | awk '{print $6}')
  echo "RESULT pass=${ok:-0} fail=$(( ${tot:-0} - ${ok:-0} )) skip=0"
}
build_libevent() {
  at_configure --disable-openssl --disable-shared --disable-samples
  make -j$JOBS; make install DESTDIR=$SYSROOT
}
test_libevent() { am_check; }
build_tmux() {
  PKG_CONFIG=$T-pkg-config at_configure CPPFLAGS="-I$SYSROOT/usr/include/ncursesw"
  make -j$JOBS
}
test_tmux() {
  # tmux's regress/ needs a terminal; a detached server, a session, a command, its output
  S=$PWD/tsock; rm -f "$S"
  ./tmux -S "$S" -f /dev/null new-session -d -x 80 -y 24 'printf "hello from tmux\n"; sleep 30'
  sleep 2
  out=$(./tmux -S "$S" capture-pane -p | head -1)
  ./tmux -S "$S" kill-server
  echo "pane: $out"
  [ "$out" = "hello from tmux" ] && echo "RESULT pass=1 fail=0 skip=0" || echo "RESULT pass=0 fail=1 skip=0"
}
build_vim() {
  cd src
  vim_cv_toupper_broken=no vim_cv_terminfo=yes vim_cv_tgetent=zero vim_cv_getcwd_broken=no \
  vim_cv_stat_ignores_slash=no vim_cv_memmove_handles_overlap=yes \
    ./configure --host=$T --prefix=/usr --with-features=normal --enable-gui=no --without-x \
      --with-tlib=ncursesw --disable-nls --disable-libsodium CPPFLAGS="-I$SYSROOT/usr/include/ncursesw"
  make -j$JOBS
}
test_vim() {
  # the old-style tests (testdir) need a terminal; run vim's script tests in Ex mode
  cd src/testdir
  timeout ${TMO:-3600} make -k -j1 VIMPROG=../vim test_vim9_expr.res test_functions.res test_listdict.res test_regexp_utf8.res \
    test_string.res test_sort.res test_eval_stuff.res test_let.res > ../../t.out 2>&1; cd ../..
  tail -20 t.out
  p=$(grep -c '^Executed' t.out); f=$(grep -cE '^(Found errors|FAILED)' t.out)
  echo "RESULT pass=$p fail=$f skip=0"
}
build_fmt() {
  # fmt 11.0.2's color.h uses std::copy without <algorithm>, which libc++ 22
  # no longer brings in transitively (fixed upstream later); its tests
  # specialize std::is_floating_point, which libc++ 22 rejects
  cmake -S . -B b -G Ninja -DCMAKE_TOOLCHAIN_FILE=$GEXCC_PREFIX/share/gexcc/toolchain.cmake -DCMAKE_BUILD_TYPE=Release -DFMT_TEST=ON \
    -DCMAKE_CXX_FLAGS="-include algorithm -Wno-invalid-specialization"
  ninja -C b -j$JOBS
}
test_fmt() { (cd b && timeout ${TMO:-3600} ctest -j$JOBS --output-on-failure) > ct.out 2>&1; tail -30 ct.out; ctest_result ct.out; }
build_perl() {
  # Configure as a native build: its try programs run (under gexc). Library
  # and header searches only in the sysroot, so host libraries (gdbm, db)
  # are not picked up. Extensions static (no dlopen of side modules).
  sh Configure -des -Dcc=gexcc -Dld=gexcc -Dar=gexcc-ar -Dranlib=gexcc-ranlib -Dnm=gexcc-nm -Uusenm \
    -Dprefix=/usr -Uusedl -Dlibpth="$SYSROOT/lib $SYSROOT/usr/lib" -Dloclibpth= -Dlocincpth= -Dglibpth= \
    -Dusrinc="$SYSROOT/include" -Doptimize=-O2 -Accflags=-D_GNU_SOURCE -Dman1dir=none -Dman3dir=none
  make -j$JOBS
}
test_perl() {
  TEST_JOBS=$JOBS timeout ${TMO:-7200} make test_harness > t.out 2>&1; tail -40 t.out
  files=$(sed -n 's/^Files=\([0-9]*\),.*/\1/p' t.out | tail -1)
  f=$(sed -n 's/^Failed \([0-9]*\)\/[0-9]* test programs.*/\1/p' t.out | tail -1)
  echo "RESULT pass=$(( ${files:-0} - ${f:-0} )) fail=${f:-0} skip=0"
}
build_python3() {
  # a native-style configure (its checks run under gexc); extension modules
  # static, as for wasm; libraries from the sysroot only
  MODULE_BUILDTYPE=static PKG_CONFIG=$T-pkg-config CC=gexcc CXX=gexc++ AR=gexcc-ar RANLIB=gexcc-ranlib \
    ./configure --prefix=/usr --disable-shared --without-ensurepip --disable-test-modules \
      --with-pkg-config=yes ac_cv_file__dev_ptmx=yes ac_cv_file__dev_ptc=no
  make -j$JOBS
}
test_python3() {
  timeout ${TMO:-7200} ./python -m test -j$JOBS --timeout 600 > t.out 2>&1; tail -60 t.out
  ok=$(sed -n 's/^\([0-9]*\) tests* OK\./\1/p' t.out | tail -1)
  f=$(sed -n 's/^\([0-9]*\) tests* failed:/\1/p' t.out | tail -1)
  e=$(sed -n 's/^\([0-9]*\) tests* failed again:/\1/p' t.out | tail -1)
  sk=$(sed -n 's/^\([0-9]*\) tests* skipped:/\1/p' t.out | tail -1)
  echo "RESULT pass=${ok:-0} fail=${f:-0} skip=${sk:-0}"
}
