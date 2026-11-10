/* Cast emulation with functions wider than binaryen's default 16 parameters:
 * one of 19 called through a pointer of another type, and one of 26 that is
 * only held in the table (its address taken, never called through it, as
 * GTK's pixbuf scalers are). binaryen asks for the call's width but builds the
 * table function's thunk at whatever width it was given, so gexcc gives it
 * the width of the widest function in a table. */
#include <stdio.h>

typedef long (*narrow_t)(int, int, int, int, int, int, int, int, int, int, int, int, int,
			 int, int, int, int, int, int);

static int sum19(int a, int b, int c, int d, int e, int f, int g, int h, int i, int j,
		 int k, int l, int m, int n, int o, int p, int q, int r, int s)
{
	return a + b + c + d + e + f + g + h + i + j + k + l + m + n + o + p + q + r + s;
}

int sum26(int a, int b, int c, int d, int e, int f, int g, int h, int i, int j, int k,
	  int l, int m, int n, int o, int p, int q, int r, int s, int t, int u, int v,
	  int w, int x, int y, int z)
{
	return a + b + c + d + e + f + g + h + i + j + k + l + m + n + o + p + q + r + s +
	       t + u + v + w + x + y + z;
}

void *volatile kept;

int main(void)
{
	kept = (void *)sum26;
	volatile narrow_t n = (narrow_t)sum19;
	printf("19: %ld\n", n(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19));
	printf("26: %d\n", sum26(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
				  20, 21, 22, 23, 24, 25, 26));
	printf("kept: %d\n", kept != 0);
	return 0;
}
