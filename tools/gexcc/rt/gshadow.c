// <gshadow.h> (include/gshadow.h) over /etc/gshadow, as glibc's: lines of
// name:password:admin,...:member,... The _r functions answer 0 with a
// result, 0 with none (not there), ERANGE (BUF too small) or another errno;
// the others keep their answer in a static buffer, and set errno to ENOENT
// at the end of the file.
#include <errno.h>
#include "../include/gshadow.h"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// LINE's fields into BUF: the line's copy, then the two lists' pointers
int sgetsgent_r(const char *line, struct sgrp *res, char *buf, size_t len, struct sgrp **out) {
    *out = NULL;
    size_t n = strcspn(line, "\n");
    if (n + 1 > len)
        return ERANGE;
    memcpy(buf, line, n);
    buf[n] = 0;
    char *f[4], *p = buf;
    for (int i = 0; i < 4; i++) {
        f[i] = p;
        char *c = i < 3 ? strchr(p, ':') : NULL;
        if (i < 3 && !c)
            return EINVAL;
        if (c) {
            *c = 0;
            p = c + 1;
        }
    }
    // pointer slots after the text, aligned
    uintptr_t at = ((uintptr_t)(buf + n + 1) + sizeof(char *) - 1) & ~(uintptr_t)(sizeof(char *) - 1);
    char **slot = (char **)at, **end = (char **)(buf + len);
    char **lists[2];
    for (int l = 0; l < 2; l++) {
        lists[l] = slot;
        char *s = f[2 + l];
        while (*s) {
            if (slot >= end)
                return ERANGE;
            *slot++ = s;
            char *c = strchr(s, ',');
            if (!c)
                break;
            *c = 0;
            s = c + 1;
        }
        if (slot >= end)
            return ERANGE;
        *slot++ = NULL;
    }
    res->sg_namp = f[0];
    res->sg_passwd = f[1];
    res->sg_adm = lists[0];
    res->sg_mem = lists[1];
    *out = res;
    return 0;
}

int fgetsgent_r(FILE *stream, struct sgrp *res, char *buf, size_t len, struct sgrp **out) {
    *out = NULL;
    char *line = NULL;
    size_t cap = 0;
    for (;;) {
        long pos = ftell(stream);
        ssize_t r = getline(&line, &cap, stream);
        if (r < 0) {
            free(line);
            return ENOENT;
        }
        if (line[0] == '\n' || line[0] == '#' || line[0] == 0)
            continue;
        int e = sgetsgent_r(line, res, buf, len, out);
        if (e == EINVAL)
            continue; // not a gshadow line: passed over, as glibc does
        if (e == ERANGE && pos >= 0)
            fseek(stream, pos, SEEK_SET); // the caller retries with more room
        free(line);
        return e;
    }
}

static FILE *db;

void setsgent(void) {
    if (db)
        rewind(db);
}

void endsgent(void) {
    if (db)
        fclose(db);
    db = NULL;
}

int getsgent_r(struct sgrp *res, char *buf, size_t len, struct sgrp **out) {
    *out = NULL;
    if (!db && !(db = fopen(GSHADOW, "re")))
        return errno;
    return fgetsgent_r(db, res, buf, len, out);
}

int getsgnam_r(const char *name, struct sgrp *res, char *buf, size_t len, struct sgrp **out) {
    *out = NULL;
    FILE *f = fopen(GSHADOW, "re");
    if (!f)
        return errno == ENOENT ? 0 : errno;
    int e;
    while ((e = fgetsgent_r(f, res, buf, len, out)) == 0)
        if (strcmp(res->sg_namp, name) == 0)
            break;
    fclose(f);
    if (e == ENOENT) {
        *out = NULL;
        return 0;
    }
    return e;
}

// the static answers: a buffer grown until the line fits
static struct sgrp one;
static char *sbuf;
static size_t slen;

static struct sgrp *grow(int (*get)(void *, struct sgrp *, char *, size_t, struct sgrp **), void *arg) {
    struct sgrp *out;
    for (;;) {
        if (!sbuf) {
            slen = 1024;
            if (!(sbuf = malloc(slen)))
                return NULL;
        }
        int e = get(arg, &one, sbuf, slen, &out);
        if (e != ERANGE) {
            if (e)
                errno = e;
            return out;
        }
        char *n = realloc(sbuf, slen * 2);
        if (!n)
            return NULL;
        sbuf = n;
        slen *= 2;
    }
}

static int via_line(void *a, struct sgrp *r, char *b, size_t l, struct sgrp **o) { return sgetsgent_r(a, r, b, l, o); }
static int via_file(void *a, struct sgrp *r, char *b, size_t l, struct sgrp **o) { return fgetsgent_r(a, r, b, l, o); }
static int via_db(void *a, struct sgrp *r, char *b, size_t l, struct sgrp **o) { (void)a; return getsgent_r(r, b, l, o); }
static int via_name(void *a, struct sgrp *r, char *b, size_t l, struct sgrp **o) {
    int e = getsgnam_r(a, r, b, l, o);
    return e == 0 && !*o ? ENOENT : e;
}

struct sgrp *sgetsgent(const char *line) { return grow(via_line, (void *)line); }
struct sgrp *fgetsgent(FILE *stream) { return grow(via_file, stream); }
struct sgrp *getsgent(void) { return grow(via_db, NULL); }
struct sgrp *getsgnam(const char *name) { return grow(via_name, (void *)name); }

static int putlist(char **l, FILE *f) {
    for (int i = 0; l && l[i]; i++)
        if (fprintf(f, "%s%s", i ? "," : "", l[i]) < 0)
            return -1;
    return 0;
}

int putsgent(const struct sgrp *g, FILE *f) {
    if (!g || !f || !g->sg_namp) {
        errno = EINVAL;
        return -1;
    }
    flockfile(f);
    int r = fprintf(f, "%s:%s:", g->sg_namp, g->sg_passwd ? g->sg_passwd : "") < 0 || putlist(g->sg_adm, f) < 0 ||
            fputc(':', f) == EOF || putlist(g->sg_mem, f) < 0 || fputc('\n', f) == EOF;
    funlockfile(f);
    return r ? -1 : 0;
}
