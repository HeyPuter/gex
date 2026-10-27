/* musl's <libgen.h>; its POSIX basename over the GNU one <string.h> names
 * under _GNU_SOURCE, as glibc has it. */
#ifdef basename
#undef basename
#endif
#include_next <libgen.h>
