/* Stands in for libxcrypt's libcrypt (a library of its own outside libc's
 * directory). */
const char *crypt_gensalt(const char *prefix, unsigned long count, const char *rbytes, int n)
{
	(void)count; (void)rbytes; (void)n;
	return prefix;
}
