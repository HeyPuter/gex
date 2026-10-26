#!/bin/bash
# item 3 for sqlite: Debian's libsqlite3 as an LP64 wasm library that
# replaces libsqlite3.so.0 under an aarch64 program (the sqlite3 shell),
# built from the amalgamation with Debian's options, on the LP64 libc.
#
# sqlite's API is callbacks both ways: a caller's function pointers
# (sqlite3_exec's callback, a virtual table module, a vfs) are stored and
# called by sqlite, and sqlite's own (the unix vfs's methods) are called by
# guest code. The GexA64Icall pass (a64icall/) makes every indirect call
# check for a guest address and gives each address-taken function an entry
# thunk the shell binds at its table index. Variadic exports and va_list
# arguments go through gexva.h.
#
#   build-sqlite.sh OUT   (SQLITE_SRC: Debian's sqlite3 source, unpacked;
#                          GEXCC_PREFIX; A64_SYSROOT; all required)
set -eu
H=$(cd "$(dirname "$0")" && pwd); O=$(realpath -m "$1"); mkdir -p "$O/sqlite"
S=${SQLITE_SRC:?the Debian sqlite3 source (3.46.1), unpacked}
OPT=${GEXCC_PREFIX:?}/sysroot/binaryen/bin/wasm-opt
: "${A64_SYSROOT:?the LP64 libc (sysroot.sh)}"
. "$H/lib.sh"
lib_setup "$H" "$O/sqlite"
"$H/a64icall/build.sh" "$O/sqlite"
# debian/rules's CFLAGS, and what its configure adds
SF="-fno-strict-aliasing -DSQLITE_SECURE_DELETE -DSQLITE_ENABLE_COLUMN_METADATA \
  -DSQLITE_ENABLE_FTS3 -DSQLITE_ENABLE_FTS3_PARENTHESIS -DSQLITE_ENABLE_RTREE=1 -DSQLITE_SOUNDEX=1 \
  -DSQLITE_ENABLE_UNLOCK_NOTIFY -DSQLITE_ENABLE_DBSTAT_VTAB -DSQLITE_ENABLE_DBPAGE_VTAB \
  -DSQLITE_ALLOW_ROWID_IN_VIEW -DSQLITE_ENABLE_UPDATE_DELETE_LIMIT=1 -DSQLITE_ENABLE_LOAD_EXTENSION \
  -DSQLITE_ENABLE_JSON1 -DSQLITE_LIKE_DOESNT_MATCH_BLOBS -DSQLITE_THREADSAFE=1 \
  -DSQLITE_ENABLE_FTS3_TOKENIZER=1 -DSQLITE_USE_URI=1 -DSQLITE_MAX_SCHEMA_RETRY=25 \
  -DSQLITE_ENABLE_PREUPDATE_HOOK -DSQLITE_ENABLE_SESSION -DSQLITE_ENABLE_STMTVTAB \
  -DSQLITE_STRICT_SUBTYPE=1 -DSQLITE_MAX_VARIABLE_NUMBER=250000 \
  -DSQLITE_ENABLE_FTS4 -DSQLITE_ENABLE_FTS5 -DSQLITE_ENABLE_MATH_FUNCTIONS -DSQLITE_TEMP_STORE=1 \
  -DSQLITE_OS_UNIX=1 -DNDEBUG -DHAVE_USLEEP=1 -DHAVE_LOCALTIME_R=1 -DHAVE_GMTIME_R=1 \
  -DHAVE_FDATASYNC=1 -DHAVE_STRCHRNUL=1 -DHAVE_ISNAN=1"
PASS="-fpass-plugin=$O/sqlite/GexA64Icall.so"
clang $CF -I"$H" -c "$H/gexva.c" -o "$O/sqlite/gexva.o" &
clang $CF -c "$H/gexa.c" -o "$O/sqlite/gexa.o" &
GEXA_ICALL_STATS=1 clang $CF $SF $PASS -include "$H/gexva.h" -I"$S" -c "$S/sqlite3.c" -o "$O/sqlite/sqlite3.o"
wait
# adapters for what this build defines (sqlite3.h declares more, under #ifdefs)
llvm-nm-22 --defined-only -g "$O/sqlite/sqlite3.o" | awk '$2 == "T" && $3 ~ /^sqlite3_/ { print $3 }' > "$O/sqlite/exports"
python3 "$H/gen.py" sqlite "$O/sqlite/exports" "$S/sqlite3.h" > "$O/sqlite/adapters.c"
clang $CF $SF -include "$H/gexva.h" -I"$H" -I"$S" -c "$O/sqlite/adapters.c" -o "$O/sqlite/adapters.o"
lib_link "$O" libsqlite3.so.0 "$O"/sqlite/{sqlite3,adapters,gexva,gexa}.o
