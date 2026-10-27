/* glibc's GNU basename (include/string.h). */
#include <string.h>

char *__gexcc_gnu_basename(const char *path)
{
	const char *p = strrchr(path, '/');
	return (char *)(p ? p + 1 : path);
}
