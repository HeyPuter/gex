# W3: Wine's own Unix side as wasm32 on gex (docs/wine-native.md), built
# reproducibly from the pins in w3.pins.
#
#   make -C tools/wine-wasm            the dist and the game-ready prefix
#   make -C tools/wine-wasm dist       the Wine install (lib/wine/i386-unix/wine, ...)
#   make -C tools/wine-wasm prefix     a ready-to-run prefix, Wayland graphics (games)
#   make -C tools/wine-wasm prefix-null  the same with the null driver (tests)
#   make -C tools/wine-wasm paths      print where they are (KEY=path lines)
#   make -C tools/wine-wasm build      the configured wasm32 build tree alone
#                                      (W3_BUILD: W64's wineserver, w64-dist.sh)
#   tools/wine-wasm/run.sh EXE [args]  run a Windows program on them (run.sh -h)
#
# Every output lives in W3_CACHE under a directory named by what it is built
# from (the Wine commit, or WINE_DIR's tree hash; the sysroot's and the
# libraries' pins; gex's own inputs: these tools and guest/gl), so a rerun
# redoes only what changed and switching between sources reuses what was
# built before. Sources come from persistent bare mirrors (W3_CACHE/mirror),
# fetched from only when a pinned commit is missing: never a fresh clone.
#
# Settings (command line, environment, or tools/wine-wasm/local.mk, which is
# not tracked):
#   W3_CACHE   outputs, mirrors and downloads (default ~/.cache/gex-w3)
#   WINE_DIR   build from this Wine worktree instead of the pin: no fetch;
#              its build tree is incremental (one per WINE_DIR), its dist
#              keyed by its tree hash, uncommitted changes included
#   W3_SEED    a local clone of the Wine fork to fill the mirror from (no
#              network); MUSL_SEED the same for the musl fork
#   W3_DL_SEED directories with the pinned tarballs already downloaded
#   GEXC       an x86 gexc, for the prefix install (default: this tree's
#              target/x86/release/gexc; cargo build --release -p gexc --features x86)
#   W3_JOBS    parallel jobs (default 12); everything runs under nice -n 19

W3 := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
ROOT := $(abspath $(W3)/../..)
include $(W3)/w3.pins
-include $(W3)/local.mk

W3_CACHE ?= $(HOME)/.cache/gex-w3
W3_JOBS ?= 12
GEXC ?= $(ROOT)/target/x86/release/gexc
GEX_WINE_PREFIX ?= $(HOME)/.cache/gex/wine-$(WINE_X86_VERSION)
GEX_CRT_SRC ?= $(HOME)/emsdk/upstream/emscripten/system/lib/compiler-rt/lib/builtins
NICE := nice -n 19
C := $(W3_CACHE)
export W3_JOBS GEX_WINE_PREFIX GEX_CRT_SRC W3_DL_SEED
export FREETYPE_VERSION FREETYPE_URL FREETYPE_SHA256 WAYLAND_VERSION WAYLAND_URL WAYLAND_SHA256
export LIBXML2_VERSION LIBXML2_URL LIBXML2_SHA256 XKBCOMMON_VERSION XKBCOMMON_URL XKBCOMMON_SHA256
export MESON_VERSION MESON_URL MESON_SHA256

h = $(shell cat $(1) | sha1sum | cut -c1-$(or $(2),10))
short = $(shell printf %.12s $(1))

# The sysroot: the musl fork plus gex's libgex, compiler-rt and the ICALL pass.
SYSROOT_KEY := $(call short,$(MUSL_COMMIT))-$(call h,$(W3)/build-sysroot.sh $(W3)/flags.sh $(wildcard $(W3)/libgex/*) $(wildcard $(W3)/icall/*))
SYSROOT := $(C)/sysroot-$(SYSROOT_KEY)
MUSL_SRC := $(C)/src/musl-$(call short,$(MUSL_COMMIT))
export GEX_WASM_SYSROOT := $(SYSROOT)/sysroot

# The Unix libraries (FreeType, wayland, xkbcommon, libxml2, EGL headers).
DEPS_KEY := $(SYSROOT_KEY)-$(call h,$(W3)/w3.pins $(W3)/w3/deps.sh $(W3)/w3/ffi/ffi.c $(W3)/w3/ffi/ffi.h $(W3)/w3/oss/sys/soundcard.h $(W3)/wcc,8)
DEPS := $(C)/deps-$(DEPS_KEY)

# gex-gl's guest library (changes with gex).
GL_KEY := $(call h,$(wildcard $(ROOT)/guest/gl/*.c $(ROOT)/guest/gl/*.h $(ROOT)/guest/gl/*.inc) $(W3)/flags.sh,10)
GL_O := $(C)/gl/gexgl-$(SYSROOT_KEY)-$(GL_KEY).o

# Wine: the pin, or WINE_DIR as it is on disk.
ifdef WINE_DIR
WINE_SRC := $(abspath $(WINE_DIR))
WINE_ID := wd-$(call short,$(shell $(W3)/w3/treekey.sh $(WINE_SRC)))
WINE_BID := wdb-$(shell printf %s $(WINE_SRC) | sha1sum | cut -c1-10)
else
WINE_SRC := $(C)/src/wine-$(call short,$(WINE_COMMIT))
WINE_ID := $(call short,$(WINE_COMMIT))
WINE_BID := $(WINE_ID)
endif
TOOLS := $(C)/tools-$(WINE_BID)
BUILD := $(C)/build-$(WINE_BID)-$(DEPS_KEY)

# The dist: everything above plus these scripts (stage, link, rebase).
TOOLS_KEY := $(call h,$(filter-out %/local.mk %/w3.pins %/run.sh %/w3/units.sh,$(wildcard $(W3)/*.py $(W3)/*.sh $(W3)/*.txt $(W3)/*.reg $(W3)/wcc $(W3)/w3/*.sh)),8)
DIST_KEY := $(WINE_ID)-$(DEPS_KEY)-$(GL_KEY)-$(TOOLS_KEY)
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
src: $(WINE_SRC)/.w3-src

paths:
	@echo W3_DIST=$(DIST)
	@echo W3_PREFIX=$(PREFIX)
	@echo W3_PREFIX_NULL=$(PREFIX_NULL)
	@echo W3_WINE_SRC=$(WINE_SRC)
	@echo W3_BUILD=$(BUILD)
	@echo GEX_WASM_SYSROOT=$(GEX_WASM_SYSROOT)
	@echo W3_UNITS=$(C)/units
	@echo W3_KEY=$(DIST_KEY)

ifndef WINE_DIR
$(WINE_SRC)/.w3-src:
	W3_SEED=$(W3_SEED) $(W3)/w3/src.sh $(WINE_REPO) $(WINE_COMMIT) $(C)/mirror/wine.git $(WINE_SRC)
endif
$(MUSL_SRC)/.w3-src:
	W3_SEED=$(MUSL_SEED) $(W3)/w3/src.sh $(MUSL_REPO) $(MUSL_COMMIT) $(C)/mirror/musl.git $(MUSL_SRC)

$(SYSROOT)/.done: $(MUSL_SRC)/.w3-src
	JOBS=$(W3_JOBS) $(NICE) $(W3)/build-sysroot.sh $(SYSROOT) $(MUSL_SRC) > $(SYSROOT).log 2>&1 || { tail -5 $(SYSROOT).log; exit 1; }
	touch $@

$(GL_O): $(SYSROOT)/.done
	mkdir -p $(dir $@)
	$(NICE) $(W3)/wcc -O2 -c -o $@.tmp $(ROOT)/guest/gl/gexgl.c && mv $@.tmp $@

$(DEPS)/.done: $(SYSROOT)/.done
	W3_GL_O=$(GL_O) $(NICE) $(W3)/w3/deps.sh $(DEPS) $(C)/dl > $(DEPS).log 2>&1 || { tail -5 $(DEPS).log; exit 1; }
	touch $@

# Wine's host tools and wasm32 tree. With WINE_DIR the stamp names the tree
# hash: an unchanged tree skips make, a changed one makes incrementally.
$(TOOLS)/.done-$(WINE_ID): $(if $(WINE_DIR),,$(WINE_SRC)/.w3-src)
	$(NICE) $(W3)/w3/wine-build.sh tools $(WINE_SRC) $(TOOLS)
	rm -f $(TOOLS)/.done-* && touch $@

$(BUILD)/.done-$(WINE_ID): $(TOOLS)/.done-$(WINE_ID) $(DEPS)/.done | $(GL_O)
	$(NICE) $(W3)/w3/wine-build.sh wasm $(WINE_SRC) $(TOOLS) $(BUILD) $(DEPS)
	rm -f $(BUILD)/.done-* && touch $@

$(DIST)/.done: $(BUILD)/.done-$(WINE_ID) $(GL_O)
	rm -rf $(DIST).tmp $(BARE).tmp
	$(NICE) $(W3)/w3/link-loader.sh $(BUILD) $(WINE_SRC) $(BUILD)/gexw3-obj $(DEPS) $(GL_O) $(DIST).tmp
	JOBS=$(W3_JOBS) $(NICE) $(W3)/link-wineserver.sh $(BUILD) $(WINE_SRC) $(BUILD)/gexw3-server $(DIST).tmp
	$(NICE) $(W3)/w3/stage.sh $(BUILD) $(WINE_SRC) $(DIST).tmp $(BARE).tmp
	rm -rf $(DIST) $(BARE) && mv $(BARE).tmp $(BARE) && mv $(DIST).tmp $(DIST)
	@echo "$(DIST_KEY) wine=$(WINE_ID) musl=$(MUSL_COMMIT) src=$(WINE_SRC)" > $(DIST)/.w3-build
	touch $@

$(INSTALLED)/.done: $(DIST)/.done
	@[ -x $(GEXC) ] || { echo "no x86 gexc at $(GEXC) (cargo build --release -p gexc --features x86, or GEXC=)"; exit 1; }
	$(NICE) $(W3)/w3/prefix.sh install $(BARE) $(DIST) $(GEXC) $(INSTALLED)
	touch $@

$(PREFIX)/.done: $(INSTALLED)/.done
	$(W3)/w3/prefix.sh ready $(INSTALLED) $(DIST) wayland $(PREFIX)
	touch $@

$(PREFIX_NULL)/.done: $(INSTALLED)/.done
	$(W3)/w3/prefix.sh ready $(INSTALLED) $(DIST) null $(PREFIX_NULL)
	touch $@
