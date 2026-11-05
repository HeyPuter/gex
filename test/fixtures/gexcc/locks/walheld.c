/* A reader process with two connections to a WAL database: one keeps a
 * statement open (its read transaction, and so its shared lock on a
 * wal-index read mark, stay held), the other runs fresh queries while
 * another program commits. Each fresh query must see every committed row.
 * SQLite counts its shm locks within a process, so the fresh query takes
 * no lock of its own while the other's is held: it reads the wal-index
 * header with no system call that says so (Kickoff's Favorites in
 * plasmashell, kamd the writer).
 *   cc walheld.c -lsqlite3; gexcc walheld.c -L DIR -lsqlite3 (DIR: a
 *   libsqlite3.so of the wasm32 tree); SQ=sqlite3 CLI, the writer */
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
typedef struct sqlite3 sqlite3;
typedef struct sqlite3_stmt sqlite3_stmt;
int sqlite3_open(const char *, sqlite3 **);
int sqlite3_exec(sqlite3 *, const char *, void *, void *, char **);
int sqlite3_prepare_v2(sqlite3 *, const char *, int, sqlite3_stmt **, const char **);
int sqlite3_step(sqlite3_stmt *);
int sqlite3_column_int(sqlite3_stmt *, int);
int sqlite3_finalize(sqlite3_stmt *);
int sqlite3_close(sqlite3 *);
static int count(sqlite3 *db) {
    sqlite3_stmt *s;
    sqlite3_prepare_v2(db, "select count(*) from t", -1, &s, 0);
    int n = sqlite3_step(s) == 100 ? sqlite3_column_int(s, 0) : -1;
    sqlite3_finalize(s);
    return n;
}
int main(void) {
    const char *f = "walheld.db";
    unlink(f);
    sqlite3 *w;
    sqlite3_open(f, &w);
    sqlite3_exec(w, "pragma journal_mode=wal; create table t(x); insert into t values(0)", 0, 0, 0);
    sqlite3_close(w);
    sqlite3 *a, *b;
    sqlite3_open(f, &a);
    sqlite3_open(f, &b);
    printf("b before: %d\n", count(b));
    sqlite3_stmt *held;
    sqlite3_prepare_v2(a, "select x from t", -1, &held, 0);
    printf("a steps: %d\n", sqlite3_step(held));
    fflush(stdout);
    for (int round = 1; round <= 2; round++) {
        /* the writer: another program, as kamd is to plasmashell */
        char cmd[256];
        snprintf(cmd, sizeof cmd, "%s walheld.db 'insert into t values(1)'", getenv("SQ") ? getenv("SQ") : "sqlite3");
        if (system(cmd) != 0)
            printf("writer failed\n");
        printf("b after write %d: %d\n", round, count(b));
    }
    sqlite3_finalize(held);
    printf("b once a is done: %d\n", count(b));
    sqlite3_close(a);
    sqlite3_close(b);
    unlink(f);
    return 0;
}
