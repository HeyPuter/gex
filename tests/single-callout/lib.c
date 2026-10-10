/* A function-tier unit (GEX_ELF_UNITS=libcb.so) whose only job is to call
   back into code no unit holds, so every call goes out through call_out /
   ftab_call. */
typedef int (*cb_t)(int);
__attribute__((noinline)) int libcb_run(cb_t f, int n)
{
	int s = 0;
	for (int i = 0; i < n; i++)
		s += f(i);
	return s;
}
