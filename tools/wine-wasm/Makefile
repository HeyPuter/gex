# 32-bit Wine: Wine's own Unix side as wasm32 on gex, built
# reproducibly from the pins in wine32.pins.
#
#   make -C tools/wine-wasm            the dist and the game-ready prefix
#   make -C tools/wine-wasm dist       the Wine install (lib/wine/i386-unix/wine, ...)
#   make -C tools/wine-wasm prefix     a ready-to-run prefix, Wayland graphics (games)
#   make -C tools/wine-wasm prefix-null  the same with the null driver (tests)
#   make -C tools/wine-wasm paths      print where they are (KEY=path lines)
#   make -C tools/wine-wasm build      the configured wasm32 build tree alone
#                                      (WINE32_BUILD: 64-bit Wine's wineserver, wine64-dist.sh)
#   tools/wine-wasm/run.sh EXE [args]  run a Windows program on them (run.sh -h)
#
# Every output lives in WINE32_CACHE under a directory named by what it is built
# from (the Wine commit, or WINE_DIR's tree hash; the sysroot's and the
# libraries' pins; gex's own inputs: these tools and guest/gl), so a rerun
# redoes only what changed and switching between sources reuses what was
# built before. Sources come from persistent bare mirrors (WINE32_CACHE/mirror),
# fetched from only when a pinned commit is missing: never a fresh clone.
#
# Settings (command line, environment, or tools/wine-wasm/local.mk, which is
# not tracked):
#   WINE32_CACHE   outputs, mirrors and downloads (default ~/.cache/gex-wine32)
#   WINE_DIR   build from this Wine worktree instead of the pin: no fetch;
#              its build tree is incremental (one per WINE_DIR), its dist
#              keyed by its tree hash, uncommitted changes included
#   WINE32_SEED    a local clone of the Wine fork to fill the mirror from (no
#              network); MUSL_SEED the same for the musl fork
#   WINE32_DL_SEED directories with the pinned tarballs already downloaded
#   GEXC       an x86 gexc, for the prefix install (default: this tree's
#              target/x86/release/gexc; cargo build --release -p gexc --features x86)
#   WINE32_MINGW_CC  the compiler for the prefix install's console launcher
#              (wine32/launch.c; default clang --target=i686-w64-mingw32
#              -fuse-ld=lld, which needs MinGW-w64's i686 headers and libraries)
#   WINE32_JOBS    parallel jobs (default 12); everything runs under nice -n 19

WINE32 := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
ROOT := $(abspath $(WINE32)/../..)
include $(WINE32)/wine32.pins
-include $(WINE32)/local.mk

WINE32_CACHE ?= $(HOME)/.cache/gex-wine32
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
SYSROOT_KEY := $(call short,$(MUSL_COMMIT))-$(call h,$(WINE32)/build-sysroot.sh $(WINE32)/flags.sh $(wildcard $(WINE32)/libgex/*) $(wildcard $(WINE32)/icall/*))
SYSROOT := $(C)/sysroot-$(SYSROOT_KEY)
MUSL_SRC := $(C)/src/musl-$(call short,$(MUSL_COMMIT))
export GEX_WASM_SYSROOT := $(SYSROOT)/sysroot

# The Unix libraries (FreeType, wayland, xkbcommon, libxml2, EGL headers).
DEPS_KEY := $(SYSROOT_KEY)-$(call h,$(WINE32)/wine32.pins $(WINE32)/wine32/deps.sh $(WINE32)/wine32/ffi/ffi.c $(WINE32)/wine32/ffi/ffi.h $(WINE32)/wine32/oss/sys/soundcard.h $(WINE32)/wcc,8)
DEPS := $(C)/deps-$(DEPS_KEY)

# gex-gl's guest library (changes with gex).
GL_KEY := $(call h,$(wildcard $(ROOT)/guest/gl/*.c $(ROOT)/guest/gl/*.h $(ROOT)/guest/gl/*.inc) $(WINE32)/flags.sh,10)
GL_O := $(C)/gl/gexgl-$(SYSROOT_KEY)-$(GL_KEY).o

# Wine: the pin, or WINE_DIR as it is on disk.
ifdef WINE_DIR
WINE_SRC := $(abspath $(WINE_DIR))
WINE_ID := wd-$(call short,$(shell $(WINE32)/wine32/treekey.sh $(WINE_SRC)))
WINE_BID := wdb-$(shell printf %s $(WINE_SRC) | sha1sum | cut -c1-10)
else
WINE_SRC := $(C)/src/wine-$(call short,$(WINE_COMMIT))
WINE_ID := $(call short,$(WINE_COMMIT))
WINE_BID := $(WINE_ID)
endif
TOOLS := $(C)/tools-$(WINE_BID)
BUILD := $(C)/build-$(WINE_BID)-$(DEPS_KEY)

# The dist: everything above plus these scripts (stage, link, rebase).
TOOLS_KEY := $(call h,$(filter-out %/local.mk %/wine32.pins %/run.sh %/wine32/units.sh,$(wildcard $(WINE32)/*.py $(WINE32)/*.sh $(WINE32)/*.txt $(WINE32)/*.reg $(WINE32)/wcc $(WINE32)/Makefile $(WINE32)/wine32/*.sh $(WINE32)/wine32/*.c)),8)
# WINE32_TLS=0: the loader without GnuTLS (crypt32's and schannel's dlopen of it
# then fails, as on a host without it), 1.8 MB less wasm, for a site that
# never speaks https. The Wine build is the same either way.
WINE32_TLS ?= 1
export WINE32_TLS
# WINE32_MONO=0: no Wine Mono in the dist (.NET programs then fail to start,
# as on a host without it), 210 MB less.
WINE32_MONO ?= 1
export WINE32_MONO
DIST_KEY := $(WINE_ID)-$(DEPS_KEY)-$(GL_KEY)-$(TOOLS_KEY)$(if $(filter 0,$(WINE32_TLS)),-notls)$(if $(filter 0,$(WINE32_MONO)),,-mono$(WINE_MONO_VERSION))
DIST := $(C)/dist-$(DIST_KEY)
BARE := $(C)/pfx-bare-$(DIST_KEY)
INSTALLED := $(C)/pfx-installed-$(DIST_KEY)
PREFIX := $(C)/pfx-$(DIST_KEY)-wayland
PREFIX_NULL := $(C)/pfx-$(DIST_KEY)-null

.PHONY: all dist prefix prefix-null paths src sysroot deps build
all: dist prefix
dist: $(DIST)/.done
prefix: $(PREFIX)/.done
prefix-null: $(PREFIX_NULL)/.done
sysroot: $(SYSROOT)/.done
deps: $(DEPS)/.done
build: $(BUILD)/.done-$(WINE_ID)
src: $(WINE_SRC)/.wine32-src

paths:
	@echo WINE32_DIST=$(DIST)
	@echo WINE32_PREFIX=$(PREFIX)
	@echo WINE32_PREFIX_NULL=$(PREFIX_NULL)
	@echo WINE32_PREFIX_BARE=$(BARE)
	@echo WINE32_WINE_SRC=$(WINE_SRC)
	@echo WINE32_BUILD=$(BUILD)
	@echo GEX_WASM_SYSROOT=$(GEX_WASM_SYSROOT)
	@echo WINE32_UNITS=$(C)/units
	@echo WINE32_KEY=$(DIST_KEY)

ifndef WINE_DIR
$(WINE_SRC)/.wine32-src:
	WINE32_SEED=$(WINE32_SEED) $(WINE32)/wine32/src.sh $(WINE_REPO) $(WINE_COMMIT) $(C)/mirror/wine.git $(WINE_SRC)
endif
$(MUSL_SRC)/.wine32-src:
	WINE32_SEED=$(MUSL_SEED) $(WINE32)/wine32/src.sh $(MUSL_REPO) $(MUSL_COMMIT) $(C)/mirror/musl.git $(MUSL_SRC)

$(SYSROOT)/.done: $(MUSL_SRC)/.wine32-src
	JOBS=$(WINE32_JOBS) $(NICE) $(WINE32)/build-sysroot.sh $(SYSROOT) $(MUSL_SRC) > $(SYSROOT).log 2>&1 || { tail -5 $(SYSROOT).log; exit 1; }
	touch $@

$(GL_O): $(SYSROOT)/.done
	mkdir -p $(dir $@)
	$(NICE) $(WINE32)/wcc -O2 -c -o $@.tmp $(ROOT)/guest/gl/gexgl.c && mv $@.tmp $@

$(DEPS)/.done: $(SYSROOT)/.done
	WINE32_GL_O=$(GL_O) $(NICE) $(WINE32)/wine32/deps.sh $(DEPS) $(C)/dl > $(DEPS).log 2>&1 || { tail -5 $(DEPS).log; exit 1; }
	touch $@

# Wine's host tools and wasm32 tree. With WINE_DIR the stamp names the tree
# hash: an unchanged tree skips make, a changed one makes incrementally.
$(TOOLS)/.done-$(WINE_ID): $(if $(WINE_DIR),,$(WINE_SRC)/.wine32-src)
	$(NICE) $(WINE32)/wine32/wine-build.sh tools $(WINE_SRC) $(TOOLS)
	rm -f $(TOOLS)/.done-* && touch $@

$(BUILD)/.done-$(WINE_ID): $(TOOLS)/.done-$(WINE_ID) $(DEPS)/.done | $(GL_O)
	$(NICE) $(WINE32)/wine32/wine-build.sh wasm $(WINE_SRC) $(TOOLS) $(BUILD) $(DEPS)
	rm -f $(BUILD)/.done-* && touch $@

$(DIST)/.done: $(BUILD)/.done-$(WINE_ID) $(GL_O)
	rm -rf $(DIST).tmp $(BARE).tmp
	$(NICE) $(WINE32)/wine32/link-loader.sh $(BUILD) $(WINE_SRC) $(BUILD)/gexwine32-obj $(DEPS) $(GL_O) $(DIST).tmp
	JOBS=$(WINE32_JOBS) $(NICE) $(WINE32)/link-wineserver.sh $(BUILD) $(WINE_SRC) $(BUILD)/gexwine32-server $(DIST).tmp
	$(NICE) $(WINE32)/wine32/stage.sh $(BUILD) $(WINE_SRC) $(DIST).tmp $(BARE).tmp
	$(if $(filter 0,$(WINE32_MONO)),,$(WINE32)/wine32/mono.sh $(C)/dl $(DIST).tmp)
	rm -rf $(DIST) $(BARE) && mv $(BARE).tmp $(BARE) && mv $(DIST).tmp $(DIST)
	@echo "$(DIST_KEY) wine=$(WINE_ID) musl=$(MUSL_COMMIT) src=$(WINE_SRC)" > $(DIST)/.wine32-build
	touch $@

$(INSTALLED)/.done: $(DIST)/.done
	@[ -x $(GEXC) ] || { echo "no x86 gexc at $(GEXC) (cargo build --release -p gexc --features x86, or GEXC=)"; exit 1; }
	$(NICE) $(WINE32)/wine32/prefix.sh install $(BARE) $(DIST) $(GEXC) $(INSTALLED)
	touch $@

$(PREFIX)/.done: $(INSTALLED)/.done
	$(WINE32)/wine32/prefix.sh ready $(INSTALLED) $(DIST) wayland $(PREFIX)
	touch $@

$(PREFIX_NULL)/.done: $(INSTALLED)/.done
	$(WINE32)/wine32/prefix.sh ready $(INSTALLED) $(DIST) null $(PREFIX_NULL)
	touch $@
