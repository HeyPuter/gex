# W1: the entry adapters winebuild (CPU_WASM32) generates, one per calling
# convention and argument/result kind.
@ stdcall abi_add(long long)
@ cdecl abi_sub(long long)
@ stdcall abi_mul64(int64 long)
@ cdecl abi_fma(double float double)
@ stdcall abi_half(float)
@ stdcall -arch=i386 -thiscall abi_this(ptr long)
@ stdcall -arch=i386 -fastcall abi_fast(long long long)
@ stdcall abi_large(int64)
@ stdcall abi_pair(long long)
@ stdcall abi_void(ptr)
@ cdecl abi_callback(ptr long)
@ stdcall abi_callback_std(ptr long)
@ extern abi_data
@ stub abi_missing
@ stdcall abi_forward() kernel32.GetTickCount
100 stdcall -noname abi_noname(long)
@ varargs abi_printf(str)
@ varargs abi_words(long)
