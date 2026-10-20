/* PROVIDES msvcr120.dll i386
 *
 * tests/w3/provdir.sh: a provider from outside gex (GEX_PROVIDER_DIRS).
 * abs answers 4242 whatever it is asked, so the test sees which code ran. */

static GEX_PROVIDER(p_abs) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 0, 4242u);
}
GEX_PROVIDE(p_abs, "abs")
