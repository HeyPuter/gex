/* The library both processes load: the unit cache hands the second one the
 * Module the first compiled. */
int foo_f(int x)
{
	int s = 0;
	for (int i = 0; i < x; i++) s += (i % 7) * 3;
	return s + 1;
}

int foo_g(int x)
{
	int s = 1;
	for (int i = 0; i < x; i++) s = s * 5 + (i & 3);
	return s;
}
