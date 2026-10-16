# W64.2: the entry adapters winebuild (CPU_WASM64) generates, one per Win64
# argument and result kind. x64 has one calling convention: stdcall and
# cdecl are the same.
@ stdcall abi_add(long long)
@ cdecl abi_mul64(int64 long)
@ cdecl abi_fma(double float double)
@ stdcall abi_half(float)
@ stdcall abi_mix(long double long float int64 double ptr)
@ stdcall abi_large(int64)
@ stdcall abi_small(long)
@ stdcall abi_big(ptr long)
@ stdcall abi_pair(long long)
@ stdcall abi_triple(long)
@ stdcall abi_ptr(ptr long)
@ stdcall abi_void(ptr)
@ cdecl abi_callback(ptr long)
@ cdecl abi_callback6(ptr long)
@ cdecl abi_callback_pair(ptr)
@ extern abi_data
@ stub abi_missing
@ stdcall abi_forward() kernel32.GetTickCount
100 stdcall -noname abi_noname(long)
@ varargs abi_printf(str)
@ varargs abi_quads(long)
@ cdecl abi_vprintf(str ptr)
