# The 32-bit Wine build tree 64-bit Wine's wineserver is compiled with, and
# its wasm32 sysroot and Unix libraries. Called by: wine/dist.sh (make wine),
# make sysroot, and the legacy 32-bit dist (wine/legacy32/legacy.mk).
#
#   make -C wine build      the configured wasm32 build tree (the default)
#   make -C wine sysroot    the wasm32 sysroot alone
#   make -C wine paths      print where they are (KEY=path lines)
#   make -C wine dist prefix prefix-null   LEGACY: the 32-bit dist and prefixes
#
# Pins: toolchain/pins (musl), wine.pins (WINE_COMMIT), libs.pins (the Unix
# libraries), legacy32/wine32.pins (the legacy dist's).
#
# Every output lives in WINE32_CACHE under a directory named by what it is built
# from (the Wine commit, or WINE_DIR's tree hash; the sysroot's and the
# libraries' pins; gex's own inputs: these tools and guest/gl), so a rerun
# redoes only what changed and switching between sources reuses what was
# built before. Sources come from persistent bare mirrors (WINE32_CACHE/mirror),
# fetched from only when a pinned commit is missing: never a fresh clone.
#
# Settings (command line, environment, or wine/local.mk, which is
# not tracked):
#   WINE32_CACHE   outputs, mirrors and downloads (default ~/.cache/gex/wine32, the top-level Makefile's CACHE/wine32)
#   WINE_DIR   build from this Wine worktree instead of the pin: no fetch;
#              its build tree is incremental (one per WINE_DIR), its dist
#              keyed by its tree hash, uncommitted changes included
#   WINE32_SEED    a local clone of the Wine fork to fill the mirror from (no
#              network; scripts/fetch-pinned.sh); MUSL_SEED the same for the musl fork
#   WINE32_DL_SEED directories with the pinned tarballs already downloaded
#   GEXC       an x86 gexc, for the prefix install (default: this tree's
#              target/x86/release/gexc; cargo build --release -p gexc --features x86)
#   WINE32_MINGW_CC  the compiler for the prefix install's console launcher
#              (legacy32/launch.c; default clang --target=i686-w64-mingw32
#              -fuse-ld=lld, which needs MinGW-w64's i686 headers and libraries)
#   WINE32_JOBS    parallel jobs (default 12); everything runs under nice -n 19
#   WINE32_STRIP   0: the dist's PE modules keep their names (a debug dist, -dbg)

WINE32 := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
ROOT := $(abspath $(WINE32)/..)
include $(ROOT)/toolchain/pins
include $(WINE32)/wine.pins
include $(WINE32)/libs.pins
include $(WINE32)/legacy32/wine32.pins
-include $(WINE32)/local.mk

WINE32_CACHE ?= $(HOME)/.cache/gex/wine32
WINE32_JOBS ?= 12
GEXC ?= $(ROOT)/target/x86/release/gexc
GEX_WINE_PREFIX ?= $(HOME)/.cache/gex/wine-$(WINE_X86_VERSION)
GEX_CRT_SRC ?= $(HOME)/emsdk/upstream/emscripten/system/lib/compiler-rt/lib/builtins
NICE := nice -n 19
C := $(WINE32_CACHE)
export WINE32_JOBS GEX_WINE_PREFIX GEX_CRT_SRC WINE32_DL_SEED
# wcc: the Wine source and this cache by fixed names in what it compiles
# (every recipe's, the loader's and server's links too), no host path.
GEX_WCC_MAP = $(WINE_SRC)=/wine:$(C)=/cache
export GEX_WCC_MAP
export FREETYPE_VERSION FREETYPE_URL FREETYPE_SHA256 WAYLAND_VERSION WAYLAND_URL WAYLAND_SHA256
export LIBXML2_VERSION LIBXML2_URL LIBXML2_SHA256 XKBCOMMON_VERSION XKBCOMMON_URL XKBCOMMON_SHA256
export MESON_VERSION MESON_URL MESON_SHA256
export WINE_MONO_VERSION WINE_MONO_URL WINE_MONO_SHA256
export NETTLE_VERSION NETTLE_URL NETTLE_SHA256 GNUTLS_VERSION GNUTLS_URL GNUTLS_SHA256 CACERT_URL CACERT_SHA256

h = $(shell cat $(1) | sha1sum | cut -c1-$(or $(2),10))
short = $(shell printf %.12s $(1))

# The sysroot: the musl fork plus gex's libgex, compiler-rt and the ICALL pass.
SYSROOT_KEY := $(call short,$(MUSL_COMMIT))-$(call h,$(WINE32)/../toolchain/sysroot/build32.sh $(WINE32)/../toolchain/sysroot/flags.sh $(wildcard $(WINE32)/../toolchain/sysroot/libgex/*) $(wildcard $(WINE32)/../toolchain/sysroot/icall/*))
SYSROOT := $(C)/sysroot-$(SYSROOT_KEY)
MUSL_SRC := $(C)/src/musl-$(call short,$(MUSL_COMMIT))
export GEX_WASM_SYSROOT := $(SYSROOT)/sysroot

# The Unix libraries (FreeType, wayland, xkbcommon, libxml2, EGL headers).
DEPS_KEY := $(SYSROOT_KEY)-$(call h,$(WINE32)/libs.pins $(WINE32)/deps32.sh $(WINE32)/ffi32/ffi.c $(WINE32)/ffi32/ffi.h $(WINE32)/oss/sys/soundcard.h $(WINE32)/wcc,8)
DEPS := $(C)/deps-$(DEPS_KEY)

# gex-gl's guest library (changes with gex).
GL_KEY := $(call h,$(wildcard $(ROOT)/guest/gl/*.c $(ROOT)/guest/gl/*.h $(ROOT)/guest/gl/*.inc) $(WINE32)/../toolchain/sysroot/flags.sh,10)
GL_O := $(C)/gl/gexgl-$(SYSROOT_KEY)-$(GL_KEY).o

# Wine: the pin, or WINE_DIR as it is on disk.
ifdef WINE_DIR
WINE_SRC := $(abspath $(WINE_DIR))
WINE_ID := wd-$(call short,$(shell $(WINE32)/legacy32/treekey.sh $(WINE_SRC)))
WINE_BID := wdb-$(shell printf %s $(WINE_SRC) | sha1sum | cut -c1-10)
else
WINE_SRC := $(C)/src/wine-$(call short,$(WINE_COMMIT))
WINE_ID := $(call short,$(WINE_COMMIT))
WINE_BID := $(WINE_ID)
endif
TOOLS := $(C)/tools-$(WINE_BID)
# (the tree's configure options are in build32.sh: a tree per set of them)
BUILD := $(C)/build-$(WINE_BID)-$(DEPS_KEY)-$(call h,$(WINE32)/build32.sh,6)

# The legacy 32-bit dist and prefixes (make wine32): wine/legacy32/legacy.mk,
# included at the end.

.PHONY: build paths src sysroot deps
.DEFAULT_GOAL := build
sysroot: $(SYSROOT)/.done
deps: $(DEPS)/.done
build: $(BUILD)/.done-$(WINE_ID)
src: $(WINE_SRC)/.pinned

paths:
	@echo WINE32_DIST=$(DIST)
	@echo WINE32_PREFIX=$(PREFIX)
	@echo WINE32_PREFIX_NULL=$(PREFIX_NULL)
	@echo WINE32_PREFIX_BARE=$(BARE)
	@echo WINE32_WINE_SRC=$(WINE_SRC)
	@echo WINE32_BUILD=$(BUILD)
	@echo GEX_WASM_SYSROOT=$(GEX_WASM_SYSROOT)
	@echo WINE32_DEPS=$(DEPS)
	@echo WINE32_UNITS=$(C)/units
	@echo WINE32_KEY=$(DIST_KEY)

ifndef WINE_DIR
$(WINE_SRC)/.pinned:
	GEX_SRC_SEED=$(WINE32_SEED) $(WINE32)/../scripts/fetch-pinned.sh $(WINE_REPO) $(WINE_COMMIT) $(C)/mirror/wine.git $(WINE_SRC)
endif
$(MUSL_SRC)/.pinned:
	GEX_SRC_SEED=$(MUSL_SEED) $(WINE32)/../scripts/fetch-pinned.sh $(MUSL_REPO) $(MUSL_COMMIT) $(C)/mirror/musl.git $(MUSL_SRC)

$(SYSROOT)/.done: | $(MUSL_SRC)/.pinned
	JOBS=$(WINE32_JOBS) $(NICE) $(WINE32)/../toolchain/sysroot/build32.sh $(SYSROOT) $(MUSL_SRC) > $(SYSROOT).log 2>&1 || { tail -5 $(SYSROOT).log; exit 1; }
	touch $@

$(GL_O): $(SYSROOT)/.done
	mkdir -p $(dir $@)
	$(NICE) $(WINE32)/wcc -O2 -c -o $@.tmp $(ROOT)/guest/gl/gexgl.c && mv $@.tmp $@

$(DEPS)/.done: $(SYSROOT)/.done | $(GL_O)
	WINE32_GL_O=$(GL_O) $(NICE) $(WINE32)/deps32.sh $(DEPS) $(C)/dl > $(DEPS).log 2>&1 || { tail -5 $(DEPS).log; exit 1; }
	touch $@

# Wine's host tools and wasm32 tree. With WINE_DIR the stamp names the tree
# hash: an unchanged tree skips make, a changed one makes incrementally.
$(TOOLS)/.done-$(WINE_ID): | $(if $(WINE_DIR),,$(WINE_SRC)/.pinned)
	$(NICE) $(WINE32)/build32.sh tools $(WINE_SRC) $(TOOLS)
	rm -f $(TOOLS)/.done-* && touch $@

$(BUILD)/.done-$(WINE_ID): $(TOOLS)/.done-$(WINE_ID) $(DEPS)/.done | $(GL_O)
	$(NICE) $(WINE32)/build32.sh wasm $(WINE_SRC) $(TOOLS) $(BUILD) $(DEPS)
	rm -f $(BUILD)/.done-* && touch $@

include $(WINE32)/legacy32/legacy.mk
