/* sys/param.h for Windows programs (gexcc-win), as MinGW's */
#pragma once
#include <sys/types.h>
#include <limits.h>
#ifndef NBBY
#define NBBY 8
#endif
#ifndef MAXPATHLEN
#define MAXPATHLEN 260
#endif
#ifndef PATH_MAX
#define PATH_MAX 260
#endif
#ifndef BYTE_ORDER
#define LITTLE_ENDIAN 1234
#define BIG_ENDIAN 4321
#define BYTE_ORDER LITTLE_ENDIAN
#endif
