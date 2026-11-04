/* strings.h for Windows programs (gexcc-win): the BSD names on the msvcrt ones */
#pragma once
#include <string.h>
#ifndef strcasecmp
#define strcasecmp _stricmp
#define strncasecmp _strnicmp
#endif
