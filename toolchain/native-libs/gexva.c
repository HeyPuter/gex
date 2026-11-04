/* gexva.h's functions. The announcement is per thread (tls.s's globals). */
#include "native-libs.h"
#include "gexva.h"
__attribute__((import_module("gex"), import_name("provider_abi"))) uint64_t gexp_host_abi(unsigned which);

void a64_va_set(uint64_t name, uint32_t counts);
uint64_t a64_va_name(void);
uint32_t a64_va_counts(void);

void gexva_pending(const char *name, unsigned ngr, unsigned nvr, unsigned nstk)
{
	a64_va_set((uint64_t)(uintptr_t)name, ngr | nvr << 8 | nstk << 16);
}

void gexva_done(void) { a64_va_set(0, 0); }

int gexva_take(gexva_list *ap, const char *func)
{
	uint64_t n = a64_va_name();
	if (__builtin_expect(!n, 1) || __builtin_strcmp((const char *)(uintptr_t)n, func))
		return 0;
	uint32_t c = a64_va_counts();
	a64_va_set(0, 0);
	unsigned ngr = c & 0xff, nvr = c >> 8 & 0xff, nstk = c >> 16;
	int ctx = a64_ctx();
	for (int i = 0; i < 8; i++)
		ap->x[i] = A64_X(ctx, i);
	static uint64_t ctx_v;
	if (__builtin_expect(!ctx_v, 0))
		ctx_v = gexp_host_abi(2);
	for (int i = 0; i < 16; i++)
		ap->v[i] = *(volatile uint64_t *)(uintptr_t)((uint32_t)ctx + ctx_v + 8 * i);
	ap->gr_top = (uint64_t)(uintptr_t)&ap->x[8];
	ap->vr_top = (uint64_t)(uintptr_t)&ap->v[16];
	ap->gr_offs = ngr >= 8 ? 0 : -8 * (int)(8 - ngr);
	ap->vr_offs = nvr >= 8 ? 0 : -16 * (int)(8 - nvr);
	ap->stack = A64_X(ctx, A64_SP) + 8 * nstk;
	return 1;
}

gexva_list gexva_guest(uint64_t p)
{
	gexva_list ap;
	volatile uint64_t *g = (volatile uint64_t *)(uintptr_t)p;
	ap.kind = 1;
	ap.stack = g[0];
	ap.gr_top = g[1];
	ap.vr_top = g[2];
	ap.gr_offs = (int32_t)(uint32_t)g[3];
	ap.vr_offs = (int32_t)(uint32_t)(g[3] >> 32);
	return ap;
}
