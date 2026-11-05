/* Calls into function-tier units through their bridges: libb.so's one and
   two alternately from one call site, then liba.so's (dlopened), then
   libb's again once liba.so's code is unmapped (its pages retired: musl's
   dlclose keeps a library mapped, so the program unmaps it itself). */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <link.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

int one(int);
int two(int);

/* One call site for both targets. */
__attribute__((noinline)) static long run(int (*a)(int), int (*b)(int), int n)
{
	long s = 0;
	for (int i = 0; i < n; i++)
		s += ((i & 1) ? b : a)(i);
	return s;
}

/* liba.so's executable segments, unmapped. */
static int unmap_code(struct dl_phdr_info *info, size_t size, void *n)
{
	(void)size;
	if (!strstr(info->dlpi_name, "/liba.so"))
		return 0;
	for (int i = 0; i < info->dlpi_phnum; i++) {
		const ElfW(Phdr) *p = &info->dlpi_phdr[i];
		if (p->p_type != PT_LOAD || !(p->p_flags & PF_X))
			continue;
		unsigned long lo = (info->dlpi_addr + p->p_vaddr) & ~4095ul;
		unsigned long hi = (info->dlpi_addr + p->p_vaddr + p->p_memsz + 4095) & ~4095ul;
		if (munmap((void *)lo, hi - lo) == 0)
			++*(int *)n;
	}
	return 0;
}

int main(int argc, char **argv)
{
	char path[4096];
	setvbuf(stdout, NULL, _IOLBF, 0);
	printf("libb %ld\n", run(one, two, 20000));
	snprintf(path, sizeof path, "%s/liba.so", argc > 1 ? argv[1] : ".");
	void *h = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!h) { printf("dlopen %s: %s\n", path, dlerror()); return 1; }
	int (*a)(int) = (int (*)(int))dlsym(h, "one"), (*b)(int) = (int (*)(int))dlsym(h, "two");
	printf("liba %ld\n", run(a, b, 20000));
	int n = 0;
	dl_iterate_phdr(unmap_code, &n);
	printf("liba code unmapped: %s\n", n ? "yes" : "no");
	printf("libb after liba unmapped %ld\n", run(one, two, 20000));
	printf("libb last %ld\n", run(two, one, 20000));
	/* Not exit: it would run liba.so's finalizers, unmapped. */
	_exit(0);
}
