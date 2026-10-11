# Run by tests/gexcc/item3/run.sh's guest shell (one process tree): every
# item-3 check, its JSON lines and the sqlite3 shell's output on stdout.
for c in "zt check" "zt checkgz" "zstt check" "zstt checkd" "zstt checkm"; do
  echo "== $c"
  /t/$c
  echo "== rc $?"
done
echo "== sqlite3 sq.sql"
/usr/bin/sqlite3 :memory: ".read /t/sq.sql"
echo "== rc $?"
rm -f /tmp/item3-cb.db
echo "== sqlite3 cb.sql"
/usr/bin/sqlite3 :memory: ".read /t/cb.sql"
echo "== rc $?"
