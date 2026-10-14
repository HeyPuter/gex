/* msvcr120's heap as a program sees it (tests/w3/crtheap.sh): the exports
 * gex's crt_heap_wine provider serves, called through GetProcAddress, with
 * every answer printed that Wine's own heap code fixes (sizes, contents,
 * alignment, NULLs), and no pointer values. Its output with the provider
 * and with the provider skipped (Wine's own code) must be the same, but for
 * the "owner" line: which heap the blocks come from. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

typedef void *(__cdecl *f_malloc)(size_t);
typedef void *(__cdecl *f_calloc)(size_t, size_t);
typedef void *(__cdecl *f_realloc)(void *, size_t);
typedef void *(__cdecl *f_recalloc)(void *, size_t, size_t);
typedef void (__cdecl *f_free)(void *);
typedef size_t (__cdecl *f_msize)(void *);
typedef void *(__cdecl *f_expand)(void *, size_t);
typedef void *(__cdecl *f_amalloc)(size_t, size_t);
typedef void *(__cdecl *f_aomalloc)(size_t, size_t, size_t);
typedef void *(__cdecl *f_arealloc)(void *, size_t, size_t);
typedef void *(__cdecl *f_aorealloc)(void *, size_t, size_t, size_t);
typedef size_t (__cdecl *f_amsize)(void *, size_t, size_t);

static f_malloc p_malloc, p_new, p_newa, p_malloc_crt;
static f_calloc p_calloc, p_calloc_crt;
static f_realloc p_realloc, p_realloc_crt;
static f_recalloc p_recalloc;
static f_free p_free, p_delete, p_deletea, p_afree;
static f_msize p_msize;
static f_expand p_expand;
static f_amalloc p_amalloc;
static f_aomalloc p_aomalloc;
static f_arealloc p_arealloc;
static f_aorealloc p_aorealloc;
static f_amsize p_amsize;

static unsigned rng = 12345;
static unsigned next(void) { rng = rng * 1103515245u + 12345u; return rng >> 8; }

static void fill(unsigned char *p, size_t n, unsigned seed) { for (size_t i = 0; i < n; i++) p[i] = (unsigned char)(seed + i * 7); }
static int same(const unsigned char *p, size_t n, unsigned seed) { for (size_t i = 0; i < n; i++) if (p[i] != (unsigned char)(seed + i * 7)) return 0; return 1; }
static int zero(const unsigned char *p, size_t n) { for (size_t i = 0; i < n; i++) if (p[i]) return 0; return 1; }

int main(void)
{
    HMODULE m = LoadLibraryA("msvcr120.dll");
    static const size_t sizes[] = { 0, 1, 3, 7, 8, 15, 16, 17, 31, 32, 100, 1000, 4095, 4096, 65536, 1 << 20 };
    unsigned i;
    if (!m) { printf("crtheap: no msvcr120\n"); return 1; }
#define GET(v, t, n) if (!(v = (t)GetProcAddress(m, n))) { printf("crtheap: no %s\n", n); return 1; }
    GET(p_malloc, f_malloc, "malloc") GET(p_new, f_malloc, "??2@YAPAXI@Z") GET(p_newa, f_malloc, "??_U@YAPAXI@Z")
    GET(p_malloc_crt, f_malloc, "_malloc_crt") GET(p_calloc, f_calloc, "calloc") GET(p_calloc_crt, f_calloc, "_calloc_crt")
    GET(p_realloc, f_realloc, "realloc") GET(p_realloc_crt, f_realloc, "_realloc_crt") GET(p_recalloc, f_recalloc, "_recalloc")
    GET(p_free, f_free, "free") GET(p_delete, f_free, "??3@YAXPAX@Z") GET(p_deletea, f_free, "??_V@YAXPAX@Z")
    GET(p_afree, f_free, "_aligned_free") GET(p_msize, f_msize, "_msize") GET(p_expand, f_expand, "_expand")
    GET(p_amalloc, f_amalloc, "_aligned_malloc") GET(p_aomalloc, f_aomalloc, "_aligned_offset_malloc")
    GET(p_arealloc, f_arealloc, "_aligned_realloc") GET(p_aorealloc, f_aorealloc, "_aligned_offset_realloc")
    GET(p_amsize, f_amsize, "_aligned_msize")
    printf("msvcr120 at %p\n", (void *)m);
    {
        void *p = p_malloc(24);
        printf("owner %s\n", HeapValidate(GetProcessHeap(), 0, p) ? "wine" : "other");
        printf("block at %p, process heap %p\n", p, (void *)GetProcessHeap());
        p_free(p);
    }
    /* malloc, _msize, realloc up and down, contents kept */
    for (i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++)
    {
        size_t n = sizes[i], up = n * 2 + 5, down = n / 3;
        unsigned char *p = p_malloc(n), *q, *r;
        size_t s0 = p ? p_msize(p) : (size_t)-2;
        fill(p, n, i);
        q = p_realloc(p, up);
        size_t s1 = q ? p_msize(q) : (size_t)-2;
        int k1 = q && same(q, n, i);
        fill(q, up, i + 1);
        r = down ? p_realloc(q, down) : q;
        size_t s2 = r ? p_msize(r) : (size_t)-2;
        int k2 = r && same(r, down, i + 1);
        printf("size %u: msize %u, up %u kept %d, down %u kept %d\n", (unsigned)n, (unsigned)s0, (unsigned)s1, k1, (unsigned)s2, k2);
        p_free(r);
    }
    /* the _crt aliases */
    {
        unsigned char *p = p_malloc_crt(10), *q;
        fill(p, 10, 3);
        q = p_realloc_crt(p, 20);
        printf("crt: msize %u kept %d\n", (unsigned)p_msize(q), same(q, 10, 3));
        p_free(q);
        q = p_calloc_crt(5, 6);
        printf("calloc_crt: msize %u zero %d\n", (unsigned)p_msize(q), zero(q, 30));
        p_free(q);
    }
    /* calloc */
    {
        unsigned char *p = p_calloc(13, 7);
        printf("calloc: msize %u zero %d\n", (unsigned)p_msize(p), zero(p, 91));
        p_free(p);
        printf("calloc overflow: %s\n", p_calloc(0x10000, 0x10001) ? "block" : "NULL");
        p = p_calloc(0, 5);
        printf("calloc 0: %s msize %u\n", p ? "block" : "NULL", p ? (unsigned)p_msize(p) : 0);
        p_free(p);
    }
    /* _recalloc */
    {
        unsigned char *p = p_calloc(10, 1), *q;
        memset(p, 0xaa, 10);
        q = p_recalloc(p, 30, 1);
        int kept = 1;
        for (i = 0; i < 10; i++) kept &= q[i] == 0xaa;
        printf("recalloc: msize %u kept %d zero %d\n", (unsigned)p_msize(q), kept, zero(q + 10, 20));
        memset(q, 0xbb, 30);
        p = p_recalloc(q, 3, 4);
        printf("recalloc down: msize %u kept %d\n", (unsigned)p_msize(p), p[11] == 0xbb);
        q = p_recalloc(p, 0, 5);
        printf("recalloc 0: %s\n", q ? "block" : "NULL");
        p = p_recalloc(NULL, 4, 4);
        printf("recalloc NULL: msize %u zero %d\n", (unsigned)p_msize(p), zero(p, 16));
        p_free(p);
    }
    /* _expand */
    {
        unsigned char *p = p_malloc(100), *q;
        fill(p, 100, 9);
        q = p_expand(p, 50);
        printf("expand down: %s msize %u kept %d\n", q == p ? "in place" : q ? "moved" : "NULL", q ? (unsigned)p_msize(q) : 0, q && same(q, 50, 9));
        printf("expand NULL: %s\n", p_expand(NULL, 10) ? "block" : "NULL");
        p_free(q ? q : p);
    }
    /* realloc's edges, _msize(NULL) */
    {
        unsigned char *p = p_realloc(NULL, 10);
        printf("realloc NULL: msize %u\n", (unsigned)p_msize(p));
        printf("realloc 0: %s\n", p_realloc(p, 0) ? "block" : "NULL");
    }
    /* operator new and delete */
    {
        unsigned char *a = p_new(37), *b = p_newa(0);
        printf("new: msize %u, new[] 0: msize %u\n", (unsigned)p_msize(a), (unsigned)p_msize(b));
        p_delete(a); p_deletea(b); p_delete(NULL);
    }
    /* the aligned family */
    {
        unsigned char *p = p_amalloc(100, 64), *q;
        fill(p, 100, 5);
        printf("aligned 64: mod %u amsize %u\n", (unsigned)((ULONG_PTR)p % 64), (unsigned)p_amsize(p, 64, 0));
        q = p_arealloc(p, 300, 64);
        printf("aligned realloc: mod %u amsize %u kept %d\n", (unsigned)((ULONG_PTR)q % 64), (unsigned)p_amsize(q, 64, 0), same(q, 100, 5));
        fill(q, 300, 6);
        p = p_arealloc(q, 40, 16);
        printf("aligned realloc 16: mod %u kept %d\n", (unsigned)((ULONG_PTR)p % 16), same(p, 40, 6));
        p_afree(p);
        p = p_aomalloc(100, 32, 8);
        fill(p, 100, 7);
        printf("aligned offset: mod %u amsize %u\n", (unsigned)(((ULONG_PTR)p + 8) % 32), (unsigned)p_amsize(p, 32, 8));
        q = p_aorealloc(p, 500, 32, 8);
        printf("aligned offset realloc: mod %u amsize %u kept %d\n", (unsigned)(((ULONG_PTR)q + 8) % 32), (unsigned)p_amsize(q, 32, 8), same(q, 100, 7));
        printf("aligned bad: %s %s\n", p_amalloc(10, 3) ? "block" : "NULL", p_aorealloc(q, 10, 32, 10) ? "block" : "NULL");
        printf("aligned realloc 0: %s\n", p_arealloc(q, 0, 32) ? "block" : "NULL");
        p_afree(q);
        p_afree(NULL);
    }
    /* churn: random blocks, contents checked */
    {
        static unsigned char *blk[512];
        static size_t len[512];
        unsigned long sum = 0, bad = 0;
        for (i = 0; i < 200000; i++)
        {
            unsigned j = next() % 512, op = next() % 4;
            size_t n = (next() % 8 == 0) ? next() % 20000 : next() % 200;
            if (blk[j] && !same(blk[j], len[j], j)) bad++;
            if (op == 0 || !blk[j]) { p_free(blk[j]); blk[j] = (op & 1) ? p_new(n) : p_malloc(n); }
            else if (op == 1) { p_free(blk[j]); blk[j] = p_calloc(n, 1); if (!zero(blk[j], n)) bad++; }
            else { blk[j] = p_realloc(blk[j], n ? n : 1); if (!n) n = 1; size_t k = len[j] < n ? len[j] : n; if (!same(blk[j], k, j)) bad++; }
            len[j] = n;
            if (p_msize(blk[j]) != n) bad++;
            fill(blk[j], n, j);
            sum += n;
        }
        for (i = 0; i < 512; i++) p_free(blk[i]);
        printf("churn: %lu bytes, %lu bad\n", sum, bad);
    }
    printf("crtheap done\n");
    return 0;
}
