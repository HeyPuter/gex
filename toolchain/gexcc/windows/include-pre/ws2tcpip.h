/* ws2tcpip.h for Windows programs (gexcc-win): Wine's, with socklen_t a typedef
 * as the Windows SDK's (Wine's is a macro, which a program's own typedef of it
 * cannot follow) */
#pragma once
#include_next <ws2tcpip.h>
#if !defined(USE_WS_PREFIX) && defined(socklen_t)
#undef socklen_t
typedef int socklen_t;
#endif
