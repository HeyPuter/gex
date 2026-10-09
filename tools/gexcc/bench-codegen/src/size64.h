/* size64 variant (build.sh): LP64 widths for size_t and ptrdiff_t on wasm32 */
#undef __SIZE_TYPE__
#define __SIZE_TYPE__ long long unsigned int
#undef __PTRDIFF_TYPE__
#define __PTRDIFF_TYPE__ long long int
#undef __SIZE_MAX__
#define __SIZE_MAX__ 18446744073709551615ULL
