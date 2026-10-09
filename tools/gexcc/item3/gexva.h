/* Variadic functions an aarch64 guest calls (docs/gexcc.md §7.8), for a
 * library whose own source uses va_list (sqlite3_mprintf, sqlite3_config):
 * build-sqlite.sh force-includes this, which makes va_list a gexva_list,
 * either wasm's own (`kind` 0: the library's calls among its functions) or
 * AAPCS64's (`kind` 1): a guest's va_list (sqlite3_vmprintf's argument,
 * read as aarch64 code would), or the registers and stack of a guest's call
 * to a variadic export, which its adapter announces (gexva_pending) and the
 * export's va_start takes. */
#ifndef GEXVA_H
#define GEXVA_H
#include <stdarg.h>
#include <stdint.h>

typedef struct gexva_list {
	int kind;
	/* AAPCS64's va_list (kind 1) */
	uint64_t stack, gr_top, vr_top;
	int gr_offs, vr_offs;
	/* the registers of a guest's call, which gr_top/vr_top point past */
	uint64_t x[8], v[16];
	__builtin_va_list w;
} gexva_list;

/* The adapter of variadic export `name`: the guest's call has `ngr` named
 * integer arguments, `nvr` floating-point ones and `nstk` stack words of
 * them. gexva_done after the call. */
void gexva_pending(const char *name, unsigned ngr, unsigned nvr, unsigned nstk);
void gexva_done(void);
/* In the export's va_start: 1 (and the guest's registers in *ap) when this
 * is the call its adapter announced. */
int gexva_take(gexva_list *ap, const char *func);
/* A guest's va_list at guest address `p` (struct __va_list, 32 bytes). */
gexva_list gexva_guest(uint64_t p);

static inline uint64_t gexva_next(gexva_list *ap, int fp)
{
	if (fp) {
		int o = ap->vr_offs;
		if (o < 0) {
			ap->vr_offs = o + 16;
			if (o + 16 <= 0)
				return *(volatile uint64_t *)(uintptr_t)(ap->vr_top + o);
		}
	} else {
		int o = ap->gr_offs;
		if (o < 0) {
			ap->gr_offs = o + 8;
			if (o + 8 <= 0)
				return *(volatile uint64_t *)(uintptr_t)(ap->gr_top + o);
		}
	}
	uint64_t r = *(volatile uint64_t *)(uintptr_t)ap->stack;
	ap->stack += 8;
	return r;
}

#undef va_start
#undef va_arg
#undef va_end
#undef va_copy
#define va_list gexva_list
#define va_start(ap, last) ((ap).kind = gexva_take(&(ap), __func__), (ap).kind ? (void)0 : __builtin_va_start((ap).w, last))
#define va_end(ap) ((ap).kind ? (void)0 : __builtin_va_end((ap).w))
#define va_copy(d, s) ((d) = (s), (s).kind ? (void)0 : __builtin_va_copy((d).w, (s).w))
#define va_arg(ap, T) ({ \
	__typeof__(T) gexva_v_; \
	if ((ap).kind) { \
		uint64_t gexva_b_ = gexva_next(&(ap), __builtin_classify_type(gexva_v_) == 8); \
		__builtin_memcpy(&gexva_v_, &gexva_b_, sizeof gexva_v_); \
	} else \
		gexva_v_ = __builtin_va_arg((ap).w, __typeof__(T)); \
	gexva_v_; })
#endif
