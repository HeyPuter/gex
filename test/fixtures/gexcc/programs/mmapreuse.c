/* Big buffers mapped and freed in turn: each one reuses the hole the last
 * left (expat's test_buffer_can_grow_to_max, libpng's pngvalid). */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void)
{
	for (int i = 0; i < 8; i++) {
		char *p = malloc(1u << 30);
		char *small = malloc(64 << 10); /* kept: lands below the big one */
		printf("%d: big %s small %s\n", i, p ? "ok" : "NULL", small ? "ok" : "NULL");
		if (p)
			memset(p + (1u << 29), 1, 4096);
		free(p);
	}
	char *a = malloc(10);
	char *b = realloc(a, (1u << 30) + 5);
	printf("realloc to 1 GiB: %s\n", b ? "ok" : "NULL");
	return 0;
}
