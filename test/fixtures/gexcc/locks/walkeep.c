/* walheld.c with a writer that stays: a second process keeps its own
 * connection open across commits (as kamd does), committing one row each
 * time the reader asks. The reader keeps one connection's statement open
 * (its read mark held) and counts with the other: each count must see
 * every row committed so far. Prints the counts: 1, 2, 3, then 3. A
 * reader woken by the writer's byte takes no system call before its count:
 * it sees the commit only if its wait's end catches its mappings up.
 * Run by test/gexcc/locks-wal. */
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
int sqlite3_busy_timeout(sqlite3 *, int);
/* every connection waits out another's lock (the child's open meets the
 * parent's first query) */
static sqlite3 *open_db(const char *f) {
    sqlite3 *db;
    sqlite3_open(f, &db);
    sqlite3_busy_timeout(db, 10000);
    return db;
}
static int count(sqlite3 *db) {
    sqlite3_stmt *s;
    sqlite3_prepare_v2(db, "select count(*) from t", -1, &s, 0);
    int n = sqlite3_step(s) == 100 ? sqlite3_column_int(s, 0) : -1;
    sqlite3_finalize(s);
    return n;
}
int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    const char *f = "walkeep.db";
    unlink(f);
    sqlite3 *w;
    sqlite3_open(f, &w);
    sqlite3_exec(w, "pragma journal_mode=wal; create table t(x); insert into t values(0)", 0, 0, 0);
    sqlite3_close(w);
    int go[2], done[2];
    if (pipe(go) || pipe(done))
        return 2;
    pid_t p = fork();
    if (p == 0) {
        /* (exec-free: a forked child's own connection, opened after fork) */
        close(go[1]);
        close(done[0]);
        sqlite3 *c = open_db(f);
        count(c);
        char x;
        while (read(go[0], &x, 1) == 1) {
            sqlite3_exec(c, "insert into t values(1)", 0, 0, 0);
            write(done[1], &x, 1);
        }
        sqlite3_close(c);
        _exit(0);
    }
    sqlite3 *a = open_db(f), *b = open_db(f);
    printf("b before: %d\n", count(b));
    sqlite3_stmt *held;
    sqlite3_prepare_v2(a, "select x from t", -1, &held, 0);
    sqlite3_step(held);
    for (int round = 1; round <= 2; round++) {
        char x = 'w';
        write(go[1], &x, 1);
        read(done[0], &x, 1);
        printf("b after write %d: %d\n", round, count(b));
    }
    sqlite3_finalize(held);
    printf("b once a is done: %d\n", count(b));
    /* and a fresh connection */
    sqlite3 *n = open_db(f);
    printf("fresh: %d\n", count(n));
    close(go[1]);
    waitpid(p, 0, 0);
    sqlite3_close(n);
    sqlite3_close(a);
    sqlite3_close(b);
    unlink(f);
    return 0;
}
