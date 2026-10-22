/* What GNU gettext's configure test (AM_GNU_GETTEXT, gettext.m4) looks for
 * in the C library before it takes the library's gettext as GNU's: glibc
 * internals it only links against. musl's gettext is a whole one
 * (bindtextdomain, the .mo catalogues), so with these present a program
 * built with NLS installs and uses its translations, as on glibc; without
 * them configure turns NLS off, and a package whose install names its
 * locale files fails (aspell, hunspell).
 *
 * _nl_msg_cat_cntr counts catalogues loaded, for a program to notice a new
 * one; it stays 0. _nl_domain_bindings is glibc's list of bound domains,
 * which only the test reads; empty. _nl_expand_alias resolves a locale
 * alias; there are none here. */
int _nl_msg_cat_cntr;

void *_nl_domain_bindings;

const char *_nl_expand_alias(const char *name)
{
    (void)name;
    return 0;
}
