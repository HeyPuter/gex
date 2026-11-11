/* assert.h for Windows programs (gexcc-win): msvcrt's, with C11's static_assert
 * (no include guard: assert.h follows NDEBUG each time it is included) */
#include_next <assert.h>
#if !defined(__cplusplus) && !defined(static_assert)
#define static_assert _Static_assert
#endif
