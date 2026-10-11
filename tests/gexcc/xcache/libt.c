/* The library whose file changes between runs: VALUE is set at build time. */
int t_value(int x)
{
	int s = 0;
	for (int i = 0; i < x; i++) s += (i % 7) * VALUE;
	return s + VALUE;
}
