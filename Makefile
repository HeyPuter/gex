# gex's front door. `make help` lists the targets a newcomer needs, and
# `make help-all` lists every target. Each target that builds something
# prints the path it made on its last line.
#
# Settings (command line or environment):
#   OUT      outputs of the targets below that are not in target/ or page/
#            (default: out/ in this tree, gitignored)
#   CACHE    fetched sources and their bare mirrors (default ~/.cache/gex)
#   JOBS     parallel jobs for the long builds (default 16)
#   WINE_DIR build 64-bit Wine from this Wine worktree, not wine/wine.pins' commit
#   LLVM_SRC a checkout of LLVM_COMMIT (toolchain/pins; libc++ for gexcc); without it
#            make gexcc fetches LLVM_REPO at that commit
#   EMSDK    the emsdk whose compiler-rt builtins the sysroots are built with
#            (default ~/emsdk; GEX_CRT_SRC= names the builtins directory itself)
#   *_SEED   a local clone to fill a mirror from with no network:
#            MUSL_SEED, MUSL64_SEED, LLVM_SEED, WINE64_SEED
#   WINE32_CACHE  32-bit Wine's sysroot, libraries and build tree (default CACHE/wine32)
#   PAGE_FLAGS more page/build.sh flags for make page / page-sp (--no-assets after
#            make assets, --debug)
#   GEX_TOOLCHAIN, GEX_X86_TOOLCHAIN  musl cross toolchains to use instead of the
#            pinned ones make toolchains fetches

ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
include $(ROOT)/toolchain/pins
include $(ROOT)/wine/wine.pins
include $(ROOT)/wine/libs.pins

OUT ?= $(ROOT)/out
CACHE ?= $(HOME)/.cache/gex
JOBS ?= 16
NICE := nice -n 10
EMSDK ?= $(HOME)/emsdk
GEX_CRT_SRC ?= $(EMSDK)/upstream/emscripten/system/lib/compiler-rt/lib/builtins
export GEX_CRT_SRC
# 32-bit Wine's sysroot, libraries and build tree (wine/Makefile), under CACHE too
WINE32_CACHE ?= $(CACHE)/wine32
export WINE32_CACHE
FETCH := $(ROOT)/scripts/fetch-pinned.sh
short = $(shell printf %.12s $(1))

GEXC_A64 := $(ROOT)/target/release/gexc
GEXC_X86 := $(ROOT)/target/x86/release/gexc

MUSL_SRC := $(CACHE)/src/musl-$(call short,$(MUSL_COMMIT))
MUSL64_SRC := $(CACHE)/src/musl64-$(call short,$(MUSL64_COMMIT))
LLVM_SRC ?= $(CACHE)/src/llvm-$(call short,$(LLVM_COMMIT))
WINE64_SRC := $(if $(WINE_DIR),$(abspath $(WINE_DIR)),$(CACHE)/src/wine64-$(call short,$(WINE64_COMMIT)))

# The wasm32 sysroot is the one wine's Makefile keys and caches
# (32-bit Wine, 64-bit Wine's server and WoW64 build against the same one).
WW := $(MAKE) -s --no-print-directory -C $(ROOT)/wine
SYSROOT = $$($(WW) paths | sed -n 's/^GEX_WASM_SYSROOT=//p')
SYSROOT64 := $(OUT)/sysroot64-$(call short,$(MUSL64_COMMIT))
BUSYBOX := $(OUT)/busybox/busybox-$(BUSYBOX_VERSION)/busybox
GEXCC_PREFIX ?= $(OUT)/gexcc

.PHONY: help help-all help-tools gexc page page-sp assets mesa xwayland serve runtime \
        sysroot sysroot64 busybox gexcc gexcc-win wine wine-x64 native-libs steam-api \
        eject-page eject-site test check check-gl regen-gl regen-mesa-hot lint wine32 toolchains \
        musl-src musl64-src llvm-src wine64-src

help:
	@echo 'make gexc          gexc for AArch64, x86-64 and i386 guests (target/release, target/x86/release)'
	@echo 'make page          the page, multi-process build: hosts, guest libraries, Mesa, Xwayland, vite (page/dist)'
	@echo 'make wine          64-bit Wine with WoW64, and a prefix (prints WINE64_DIST=, WINE64_PREFIX=)'
	@echo 'make gexcc         the wasm32 C/C++ compiler, from pins (out/gexcc; GEXCC_PREFIX= elsewhere)'
	@echo 'make mesa          Mesa for the page alone'
	@echo 'make test          the runtime tests: node test/run.mjs (ARGS="..." passes options)'
	@echo 'make help-all      every target'

help-all: help
	@echo
	@echo 'make page-sp       the single-thread page (page/dist-sp)'
	@echo 'make assets        the hosts, guest libraries and rootfs alone (page/public)'
	@echo 'make xwayland      Xwayland for the page alone'
	@echo 'make toolchains    the pinned musl cross toolchains the page builds guest code with (CACHE/toolchains)'
	@echo 'make serve         serve page/dist (PORT, default 8088) and page/dist-sp (SP_PORT, 8089)'
	@echo 'make runtime       the npm package @gexemu/runtime (PKG=DIR, default out/runtime)'
	@echo 'make sysroot       the wasm32 sysroot (musl at MUSL_COMMIT); SNAPSHOT=1: the snapshot flavour'
	@echo 'make sysroot64     the LP64 sysroot (musl at MUSL64_COMMIT)'
	@echo 'make busybox       wasm32 busybox (BUSYBOX_VERSION, sha256-pinned)'
	@echo 'make gexcc-win     gexcc'"'"'s Windows x64 target, over make wine'"'"'s build tree'
	@echo 'make wine-x64      64-bit Wine without WoW64'
	@echo 'make native-libs   zlib, zstd, SQLite as LP64 wasm replacements (ZLIB_SRC, ZSTD_SRC, SQLITE_SRC)'
	@echo 'make steam-api     libsteam_api for games (STEAM_API_JSON; out/steam)'
	@echo 'make eject-page    an ejected program'"'"'s page library (DIR=, default out/eject-page)'
	@echo 'make eject-site    an ejected program'"'"'s static site (PROJECT=, SITE=)'
	@echo 'make check         cargo test, the workspace (X86=1: with the x86 features)'
	@echo 'make check-gl      the GL tables match their generators'
	@echo 'make regen-gl      rewrite the GL tables from the Khronos registry'
	@echo 'make regen-mesa-hot  rewrite ports/mesa/hot.txt (PLAIN=, LOGS=)'
	@echo 'make lint          every script is a target, called by one, or a public tool'
	@echo 'make help-tools    the tools other repos and users call by path'
	@echo 'make wine32        LEGACY: old 32-bit Wine (wine Makefile), until it is removed'

help-tools:
	@sed -n 's/^\([^#].*\)/\1/p' $(ROOT)/scripts/public-tools

# ---- gexc ----
gexc:
	cargo build --release -p gexc
	cargo build --release -p gexc --features x86 --target-dir target/x86
	@echo $(GEXC_A64) $(GEXC_X86)

# ---- the page ----
# Mesa, and the Khronos registry guest/gl/build.sh generates the GL tables from
MESA_CHECKOUT = git -C $(ROOT) submodule update --init third_party/mesa third_party/opengl-registry
# vite and the page's other npm packages, at package-lock.json's versions
NPM_CI = [ $(ROOT)/page/node_modules/.package-lock.json -nt $(ROOT)/page/package-lock.json ] || (cd $(ROOT)/page && npm ci --no-audit --no-fund)
# The page's guest-side cross compilers (toolchain/pins), unpacked once in
# CACHE/toolchains; GEX_TOOLCHAIN= / GEX_X86_TOOLCHAIN= name others.
TC := $(CACHE)/toolchains
GEX_TOOLCHAIN ?= $(TC)/aarch64-linux-musl-cross
GEX_X86_TOOLCHAIN ?= $(TC)/x86_64-linux-musl-cross
export GEX_TOOLCHAIN GEX_X86_TOOLCHAIN
toolchains: $(TC)/aarch64-linux-musl-cross/.pinned $(TC)/x86_64-linux-musl-cross/.pinned
$(TC)/%-linux-musl-cross/.pinned:
	$(ROOT)/wine/dl.sh $(MUSL_CROSS_$(call upper,$*)_URL) $(MUSL_CROSS_$(call upper,$*)_SHA256) $(CACHE)/dl
	rm -rf $(TC)/$*-linux-musl-cross && mkdir -p $(TC) && tar -C $(TC) -xzf $(CACHE)/dl/$*-linux-musl-cross.tgz
	echo $(MUSL_CROSS_$(call upper,$*)_SHA256) > $@
upper = $(subst x86_64,X86_64,$(subst aarch64,AARCH64,$(1)))

# emcc from EMSDK at toolchain/pins' EMSCRIPTEN_VERSION, first on PATH
EMCC_CHECK = v=$$(tr -d '"' < $(EMSDK)/upstream/emscripten/emscripten-version.txt 2>/dev/null); \
  [ "$$v" = "$(EMSCRIPTEN_VERSION)" ] || { echo "make: EMSDK=$(EMSDK) has Emscripten '$$v', not toolchain/pins' $(EMSCRIPTEN_VERSION) (emsdk install $(EMSCRIPTEN_VERSION))"; exit 1; }
# Mesa: only what the page loads (gex-wgpu-replay); GEX_MESA_PAGE_ONLY=0 also
# links the softpipe replay and the smokes the Mesa tests use.
GEX_MESA_PAGE_ONLY ?= 1
export GEX_MESA_PAGE_ONLY
PAGE_ENV = PATH=$(EMSDK)/upstream/emscripten:$(EMSDK)/upstream/bin:$$PATH

page: toolchains
	@$(EMCC_CHECK)
	$(MESA_CHECKOUT)
	$(NPM_CI)
	$(PAGE_ENV) $(ROOT)/page/build.sh --mp $(PAGE_FLAGS)
	@echo $(ROOT)/page/dist
page-sp: toolchains
	@$(EMCC_CHECK)
	$(MESA_CHECKOUT)
	$(NPM_CI)
	$(PAGE_ENV) $(ROOT)/page/build.sh --sp $(PAGE_FLAGS)
	@echo $(ROOT)/page/dist-sp
assets: toolchains
	$(ROOT)/page/build-assets.sh
	@echo $(ROOT)/page/public
mesa:
	$(MESA_CHECKOUT)
	@$(EMCC_CHECK)
	$(PAGE_ENV) $(ROOT)/page/build.sh --mp --gl-only
xwayland:
	@$(EMCC_CHECK)
	$(PAGE_ENV) $(ROOT)/page/build.sh --mp --x-only
PORT ?= 8088
SP_PORT ?= 8089
serve:
	node $(ROOT)/page/serve.mjs $(PORT) --sp-port $(SP_PORT)
PKG ?= $(OUT)/runtime
runtime:
	$(NPM_CI)
	$(ROOT)/page/runtime/build.sh $(PKG)
	@echo $(PKG)

# ---- pinned sources ----
musl-src:
	GEX_SRC_SEED=$(MUSL_SEED) $(FETCH) $(MUSL_REPO) $(MUSL_COMMIT) $(CACHE)/mirror/musl.git $(MUSL_SRC)
musl64-src:
	GEX_SRC_SEED=$(MUSL64_SEED) $(FETCH) $(MUSL64_REPO) $(MUSL64_COMMIT) $(CACHE)/mirror/musl64.git $(MUSL64_SRC)
# A given LLVM_SRC is used as it is (build-cxx.sh checks it is at the pin).
llvm-src:
	$(if $(filter file,$(origin LLVM_SRC)),GEX_SRC_SEED=$(LLVM_SEED) $(FETCH) --worktree $(LLVM_REPO) $(LLVM_COMMIT) $(CACHE)/mirror/llvm.git $(LLVM_SRC),@echo "LLVM from LLVM_SRC=$(LLVM_SRC)")
wine64-src:
	$(if $(WINE_DIR),@echo "64-bit Wine from WINE_DIR=$(WINE64_SRC)",GEX_SRC_SEED=$(WINE64_SEED) $(FETCH) --worktree $(WINE64_REPO) $(WINE64_COMMIT) $(CACHE)/mirror/wine64.git $(WINE64_SRC))

# ---- the toolchain ----
ifeq ($(SNAPSHOT),1)
sysroot: musl-src
	rm -rf $(OUT)/sysroot-snap.tmp
	GEX_SNAPSHOT=1 JOBS=$(JOBS) $(NICE) $(ROOT)/toolchain/sysroot/build32.sh $(OUT)/sysroot-snap.tmp $(MUSL_SRC) > $(OUT)/sysroot-snap.log 2>&1 || { tail -5 $(OUT)/sysroot-snap.log; exit 1; }
	rm -rf $(OUT)/sysroot-snap && mv $(OUT)/sysroot-snap.tmp $(OUT)/sysroot-snap
	@echo $(OUT)/sysroot-snap/sysroot
else
sysroot:
	$(WW) sysroot WINE32_JOBS=$(JOBS) MUSL_SEED=$(MUSL_SEED)
	@echo $(SYSROOT)
endif

SYSROOT64_IN := $(ROOT)/toolchain/sysroot/build64.sh $(ROOT)/toolchain/sysroot/flags64.sh $(ROOT)/wine/wine.pins \
  $(wildcard $(ROOT)/toolchain/sysroot/llp64/* $(ROOT)/toolchain/sysroot/icall/* $(ROOT)/toolchain/sysroot/libgex/* $(ROOT)/toolchain/sysroot/libgex64/*)
$(SYSROOT64)/.done: $(SYSROOT64_IN)
	$(MAKE) -s musl64-src
	mkdir -p $(OUT) && rm -rf $(SYSROOT64).tmp
	JOBS=$(JOBS) $(NICE) $(ROOT)/toolchain/sysroot/build64.sh $(SYSROOT64).tmp $(MUSL64_SRC) > $(SYSROOT64).log 2>&1 || { tail -5 $(SYSROOT64).log; exit 1; }
	rm -rf $(SYSROOT64) && mv $(SYSROOT64).tmp $(SYSROOT64) && touch $@
sysroot64: $(SYSROOT64)/.done
	@echo $(SYSROOT64)/sysroot

$(BUSYBOX): $(ROOT)/toolchain/busybox/build.sh $(ROOT)/toolchain/busybox/busybox.config
	$(MAKE) -s sysroot >/dev/null
	$(ROOT)/wine/dl.sh $(BUSYBOX_URL) $(BUSYBOX_SHA256) $(OUT)/busybox
	rm -rf $(OUT)/busybox/busybox-$(BUSYBOX_VERSION)
	tar xjf $(OUT)/busybox/$(notdir $(BUSYBOX_URL)) -C $(OUT)/busybox
	GEX_WASM_SYSROOT=$(SYSROOT) $(NICE) $(ROOT)/toolchain/busybox/build.sh $(OUT)/busybox $(BUSYBOX_VERSION) > $(OUT)/busybox/build.log 2>&1 || { tail -5 $(OUT)/busybox/build.log; exit 1; }
busybox: $(BUSYBOX)
	@echo $(BUSYBOX)

gexcc: musl-src llvm-src
	$(NICE) cargo build -q --release -p gex-dylink
	@[ -x $(GEXC_X86) ] || $(NICE) cargo build -q --release -p gexc --features x86 --target-dir target/x86
	$(ROOT)/wine/dl.sh $(BUSYBOX_URL) $(BUSYBOX_SHA256) $(CACHE)/dl
	GEXC=$(GEXC_X86) GEXC_X86=$(GEXC_X86) GEX_DYLINK=$(ROOT)/target/release/gex-dylink JOBS=$(JOBS) LLVM_SRC=$(LLVM_SRC) \
	  BUSYBOX_TAR=$(CACHE)/dl/$(notdir $(BUSYBOX_URL)) GEXCC_CACHE=$(CACHE)/gexcc $(NICE) sh $(ROOT)/toolchain/gexcc/install.sh $(GEXCC_PREFIX) $(MUSL_SRC)
	@echo $(GEXCC_PREFIX)/bin/gexcc

# ---- 64-bit Wine ----
# wine/dist.sh caches every step in CACHE/wine64 by its inputs, so a rerun
# only checks. It builds 32-bit Wine's wasm32 tree for the wineserver first
# when missing (wine `make build`).
wine-x64 wine: wine64-src sysroot64
	@mkdir -p $(OUT)
	$(if $(filter wine,$@),WINE64_WOW64=1) GEX_WASM64_SYSROOT=$(SYSROOT64)/sysroot JOBS=$(JOBS) \
	  $(ROOT)/wine/dist.sh $(WINE64_SRC) $(CACHE)/wine64 > $(OUT)/$@.env
	@cat $(OUT)/$@.env

gexcc-win: wine llvm-src
	. $(OUT)/wine.env && GEX_WASM64_SYSROOT=$(SYSROOT64)/sysroot $(NICE) bash $(ROOT)/toolchain/gexcc/windows/install.sh $(GEXCC_PREFIX)-win $$WINE64_BUILD $$WINE64_SRC
	GEX_WASM64_SYSROOT=$(SYSROOT64)/sysroot $(NICE) bash $(ROOT)/toolchain/gexcc/windows/build-cxx.sh $(GEXCC_PREFIX)-win $(LLVM_SRC)
	@echo $(GEXCC_PREFIX)-win/bin/x86_64-w64-mingw32-gcc

# ---- libraries ----
native-libs:
	@[ -n "$(ZLIB_SRC)" ] && [ -n "$(ZSTD_SRC)" ] && [ -n "$(SQLITE_SRC)" ] || \
	  { echo "make native-libs: set ZLIB_SRC, ZSTD_SRC and SQLITE_SRC (Debian's unpacked sources), with gexcc on PATH"; exit 1; }
	sh $(ROOT)/toolchain/native-libs/build.sh $(OUT)/native-libs
	sh $(ROOT)/toolchain/native-libs/build-zstd.sh $(OUT)/native-libs
	sh $(ROOT)/toolchain/native-libs/build-sqlite.sh $(OUT)/native-libs
	@echo $(OUT)/native-libs
steam-api:
	@[ -n "$(STEAM_API_JSON)" ] || { echo "make steam-api: set STEAM_API_JSON (the Steamworks SDK's steam_api.json)"; exit 1; }
	sh $(ROOT)/guest/steam/build.sh $(OUT)/steam
	@echo $(OUT)/steam

# ---- ejected programs ----
DIR ?= $(OUT)/eject-page
eject-page:
	$(ROOT)/site/build-eject-page.sh $(DIR)
	@echo $(DIR)
eject-site:
	@[ -n "$(PROJECT)" ] && [ -n "$(SITE)" ] || { echo "make eject-site: set PROJECT (an ejected project) and SITE (the output directory)"; exit 1; }
	node $(ROOT)/site/eject-site.mjs $(PROJECT) $(SITE) $(SITE_ARGS)
	@echo $(SITE)

# ---- tests and checks ----
test:
	@[ -x $(GEXC_A64) ] || $(MAKE) gexc
	$(if $(wildcard $(BUSYBOX)),BUSYBOX=$(BUSYBOX)) node $(ROOT)/test/run.mjs $(ARGS)
check:
	cargo test --release --workspace --exclude gex-kmem --exclude gex-kernel-wasm --exclude gex-browser \
	  $(if $(X86),--features gex-kernel/x86$(,)gex-xlate/x86$(,)gexc/x86$(,)gex-native/x86$(,)gex-eject/x86 --target-dir target/x86t)
, := ,
check-gl:
	git -C $(ROOT) submodule update --init third_party/opengl-registry
	python3 $(ROOT)/scripts/gl/gen_gl_compat.py --check
	python3 $(ROOT)/scripts/gl/gen_gl44_transport.py --check
regen-gl:
	git -C $(ROOT) submodule update --init third_party/opengl-registry
	python3 $(ROOT)/scripts/gl/gen_gl_compat.py
	python3 $(ROOT)/scripts/gl/gen_gl44_transport.py
regen-mesa-hot:
	@[ -n "$(PLAIN)" ] && [ -n "$(LOGS)" ] || { echo "make regen-mesa-hot: set PLAIN (gex-wgpu-replay.wasm.orig) and LOGS (profile logs)"; exit 1; }
	python3 $(ROOT)/ports/mesa/mesa-hot.py $(PLAIN) $(LOGS) -o $(ROOT)/ports/mesa/hot.txt
lint:
	$(ROOT)/scripts/lint-scripts.py

# ---- legacy ----
wine32:
	$(MAKE) -C $(ROOT)/wine dist prefix
	@$(WW) paths
