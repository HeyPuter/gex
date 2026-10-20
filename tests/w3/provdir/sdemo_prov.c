/* tests/w3/provdir.sh: a stateful provider for sdemo.dll. sd_add answers
 * a + b + 1000; sd_other is not provided (gexp__missing); its data is a
 * counter, so a second call answers one more. */
#include "gex_abi.h"
#include "provider.h"

__attribute__((import_module("gex"), import_name("provider_missing"))) void gex_provider_missing(int addr);

static int calls;

static GEX_PROVIDER(gexp_sd_add) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 0, gp_arg(ctx, 0) + gp_arg(ctx, 1) + 1000u + (uint32_t)calls++);
}
__attribute__((export_name("gexp_ptr_sd_add"))) gex_body_t gexp_ptr_sd_add(void) { return gexp_sd_add; }

static GEX_PROVIDER(gexp__missing) {
    (void)depth, (void)need, (void)ret;
    gex_provider_missing((int)GP_SELF_PC());
    __builtin_trap();
}
__attribute__((export_name("gexp_ptr__missing"))) gex_body_t gexp_ptr__missing(void) { return gexp__missing; }

/* DllMain (stdcall, three arguments): nothing to set up. */
static GEX_PROVIDER(gexp__entry) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 12, 1);
}
__attribute__((export_name("gexp_ptr__entry"))) gex_body_t gexp_ptr__entry(void) { return gexp__entry; }
