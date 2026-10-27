// glibc's fgetspent_r and sgetspent_r (include/shadow.h): lines of
// name:password:lastchg:min:max:warn:inactive:expire:flag, an empty number
// -1. They answer 0 with a result; ERANGE (BUF too small), EINVAL (not such
// a line) or, at the end of the file, ENOENT, with none, errno set to the
// same. fgetspent_r skips lines that are not entries, as glibc's does.
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <shadow.h>

static int num(char *s, long *v)
{
	if (!*s) {
		*v = -1;
		return 0;
	}
	char *e;
	errno = 0;
	*v = strtol(s, &e, 10);
	return *e || errno ? -1 : 0;
}

int sgetspent_r(const char *line, struct spwd *res, char *buf, size_t len, struct spwd **out)
{
	*out = NULL;
	size_t n = strcspn(line, "\n");
	if (n + 1 > len)
		return errno = ERANGE;
	memmove(buf, line, n);
	buf[n] = 0;
	char *f[9], *p = buf;
	int k = 0;
	for (; k < 9; k++) {
		f[k] = p;
		char *c = strchr(p, ':');
		if (!c)
			break;
		*c = 0;
		p = c + 1;
	}
	// glibc takes a line of the first two fields alone (an old-style entry)
	if (k < 8 && k != 1)
		return errno = EINVAL;
	long v[7] = {-1, -1, -1, -1, -1, -1, -1};
	if (k >= 8)
		for (int i = 0; i < 7; i++)
			if (num(f[2 + i], &v[i]))
				return errno = EINVAL;
	res->sp_namp = f[0];
	res->sp_pwdp = f[1];
	res->sp_lstchg = v[0];
	res->sp_min = v[1];
	res->sp_max = v[2];
	res->sp_warn = v[3];
	res->sp_inact = v[4];
	res->sp_expire = v[5];
	res->sp_flag = (unsigned long)v[6];
	*out = res;
	return 0;
}

int fgetspent_r(FILE *f, struct spwd *res, char *buf, size_t len, struct spwd **out)
{
	*out = NULL;
	if (len < 2)
		return errno = ERANGE;
	for (;;) {
		if (!fgets(buf, (int)len, f))
			return errno = ENOENT;
		size_t n = strlen(buf);
		if (n == len - 1 && buf[n - 1] != '\n' && !feof(f))
			return errno = ERANGE;
		char *s = buf + strspn(buf, " \t");
		if (!*s || *s == '\n' || *s == '#')
			continue;
		// parsed in place: the line moves to the start of BUF, which it is
		if (s != buf)
			memmove(buf, s, strlen(s) + 1);
		int r = sgetspent_r(buf, res, buf, len, out);
		if (r == 0)
			return 0;
		if (r == ERANGE)
			return r;
	}
}
