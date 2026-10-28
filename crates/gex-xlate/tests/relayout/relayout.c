/* The relayout test program (tests/relayout_x86.rs): functions that walk
 * structs holding pointers and longs, so an LP64 build reads them at
 * different offsets and widths than an ILP32 build would. */
#define F __attribute__((noinline, used))

struct node {
    long key;
    struct node *next;
    const char *name;
    int tag;
};

struct pair {
    struct node *a;
    struct node *b;
    long weight;
};

struct node head;

F long sum_keys(struct node *n) {
    long s = 0;
    for (; n; n = n->next)
        s += n->key;
    return s;
}

F void set_tag(struct node *n, int t) {
    for (; n; n = n->next)
        n->tag = t;
}

F const char *name_of(struct pair *p) { return p->b->name; }

F long weight_of(struct pair *p) { return p->weight + p->a->key - p->b->key; }

F struct node *nth(struct node **tab, long i) { return tab[i]; }

F long pair_key(struct pair *ps, long i) { return ps[i].b->key; }

F void link_nodes(struct node *a, struct node *b) {
    a->next = b;
    b->next = 0;
}

F struct node *first(void) { return head.next; }

F struct node *second(struct node *n) { return n->next; }

F long third_key(struct node *n) { return second(second(n))->key; }

F long count_between(struct node *lo, struct node *hi) { return hi - lo; }

void entry(void) {}
