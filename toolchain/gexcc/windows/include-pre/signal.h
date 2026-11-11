/* signal.h for Windows programs (gexcc-win): msvcrt's, with the sig_atomic_t
 * the Windows SDK's and MinGW's declare */
#pragma once
#include_next <signal.h>
#ifndef _SIG_ATOMIC_T_DEFINED
#define _SIG_ATOMIC_T_DEFINED
typedef int sig_atomic_t;
#endif
