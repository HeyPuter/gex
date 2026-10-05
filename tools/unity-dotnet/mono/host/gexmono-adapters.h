/* What the generated adapters (gen-adapters.py) include: the provider view
 * of the guest's registers and Mono's public API. */
#include <stdint.h>
#include "gexprov64.h"
#include <mono/jit/jit.h>
#include <mono/jit/mono-private-unstable.h>
#include <mono/metadata/appdomain.h>
#include <mono/metadata/assembly.h>
#include <mono/metadata/class.h>
#include <mono/metadata/debug-helpers.h>
#include <mono/metadata/environment.h>
#include <mono/metadata/exception.h>
#include <mono/metadata/image.h>
#include <mono/metadata/loader.h>
#include <mono/metadata/metadata.h>
#include <mono/metadata/mono-config.h>
#include <mono/metadata/mono-debug.h>
#include <mono/metadata/mono-gc.h>
#include <mono/metadata/mono-private-unstable.h>
#include <mono/metadata/object.h>
#include <mono/metadata/opcodes.h>
#include <mono/metadata/profiler.h>
#include <mono/metadata/reflection.h>
#include <mono/metadata/sgen-bridge.h>
#include <mono/metadata/threads.h>
#include <mono/utils/mono-counters.h>
#include <mono/utils/mono-dl-fallback.h>
#include <mono/utils/mono-error.h>
#include <mono/utils/mono-logger.h>
#include <mono/utils/mono-publib.h>

/* GEXMONO_TRACE=1: every embedding API call, with its first arguments, on
 * stderr (gexmono-provider.c). */
extern int gexmono_trace_on;
void gexmono_trace (int ctx, const char *name);
#define GEXMONO_TRACE(name) do { if (__builtin_expect (gexmono_trace_on, 0)) gexmono_trace (ctx, name); } while (0)
