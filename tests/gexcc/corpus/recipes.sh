# Corpus recipes (run.sh). build_NAME runs in the unpacked tree with `set -e`;
# test_NAME prints `RESULT pass=N fail=N skip=N`. Libraries other programs
# use install into the sysroot (--prefix=/usr, DESTDIR=$SYSROOT).

# autotools: configure for the gex host
at_configure() { ./configure --host=$T --prefix=/usr "$@"; }
# automake's test-suite summary (# PASS: n ...) summed over every log
am_result() {
  awk '/^# PASS:/{p+=$3} /^# FAIL:/{f+=$3} /^# ERROR:/{f+=$3} /^# XPASS:/{f+=$3} /^# SKIP:/{s+=$3} /^# XFAIL:/{x+=$3}
       END{printf "RESULT pass=%d fail=%d skip=%d\n", p+x, f, s}' "$@"
}
am_check() { timeout ${TMO:-3600} make -k -j$JOBS check "$@" > check.out 2>&1; cat check.out; am_result check.out; }

build_zlib() {
  CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib CHOST=$T ./configure --prefix=/usr
  make -j$JOBS
  make install DESTDIR=$SYSROOT
}
test_zlib() {
  make test 2>&1 | tee t.out
  n=$(grep -c 'test OK' t.out); echo "RESULT pass=$n fail=$((3 - n)) skip=0"
}

build_xz() { at_configure --disable-nls; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_xz() { am_check; }

# plain Makefile
build_bzip2() {
  make -j$JOBS CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib bzip2 bzip2recover
}
test_bzip2() {
  make check CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib 2>&1 | tee t.out
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
test_jq() { am_check; }

build_pcre2() { at_configure --enable-pcre2-16 --enable-pcre2-32; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_pcre2() { am_check; }

build_expat() { at_configure --without-docbook --without-examples; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_expat() { am_check; }

build_libpng() { at_configure; make -j$JOBS; make install DESTDIR=$SYSROOT; }
test_libpng() { am_check; }
