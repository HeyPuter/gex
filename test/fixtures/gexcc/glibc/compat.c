/* glibc names gexcc's headers and libgexcc.a add to musl's: GNU basename,
 * __ino_t, _PATH_WTMPX through <utmpx.h>, fts(3), fgetpwent and fgetgrent
 * in the default source, fgetspent_r and sgetspent_r. argv[1]: a directory
 * holding tree/ (two files), passwd, group and shadow. */
#define _GNU_SOURCE 1
#include <grp.h>
#include <pwd.h>
#include <shadow.h>
#include <stdio.h>
#include <string.h>
#include <utmpx.h>
#include <fts.h>

static char path[4096];

static FILE *at(const char *dir, const char *name)
{
	snprintf(path, sizeof path, "%s/%s", dir, name);
	return fopen(path, "r");
}

int main(int argc, char **argv)
{
	const char *d = argc > 1 ? argv[1] : ".";
	printf("basename [%s] [%s]\n", basename("/a/b"), basename("/a/b/"));
	__ino_t i = 0;
	__ino64_t j = 0;
	printf("ino %zu %zu\n", sizeof i, sizeof j);
	printf("wtmpx %d\n", strcmp(_PATH_WTMPX, _PATH_WTMP) == 0);

	snprintf(path, sizeof path, "%s/tree", d);
	char *roots[] = { path, 0 };
	FTS *f = fts_open(roots, FTS_PHYSICAL, 0);
	int files = 0;
	for (FTSENT *e; f && (e = fts_read(f));)
		files += e->fts_info == FTS_F;
	if (f)
		fts_close(f);
	printf("fts %d\n", files);

	FILE *pw = at(d, "passwd");
	struct passwd *p = pw ? fgetpwent(pw) : 0;
	printf("fgetpwent %s %d\n", p ? p->pw_name : "-", p ? (int)p->pw_uid : -1);
	FILE *gr = at(d, "group");
	struct group *g = gr ? fgetgrent(gr) : 0;
	printf("fgetgrent %s %d\n", g ? g->gr_name : "-", g ? (int)g->gr_gid : -1);

	char buf[512];
	struct spwd sp, *r = 0;
	FILE *sh = at(d, "shadow");
	int e = sh ? fgetspent_r(sh, &sp, buf, sizeof buf, &r) : -1;
	printf("fgetspent_r %d %s %ld\n", e, r ? r->sp_namp : "-", r ? r->sp_max : -1L);
	r = 0;
	e = sgetspent_r("bob:!:19000:0:90:7:::", &sp, buf, sizeof buf, &r);
	printf("sgetspent_r %d %s %ld\n", e, r ? r->sp_namp : "-", r ? r->sp_max : -1L);
	return 0;
}
