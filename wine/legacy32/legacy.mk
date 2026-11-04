# The legacy 32-bit Wine dist and its prefixes, over wine/Makefile's build
# tree (make wine32 at the top level, or make -C wine dist prefix prefix-null).
# wine/Makefile includes this file; it goes with wine/legacy32.

# The dist: everything above plus these scripts (stage, link, rebase).
TOOLS_KEY := $(call h,$(filter-out %/local.mk %/run.sh %/units.sh,$(wildcard $(WINE32)/*.py $(WINE32)/*.sh $(WINE32)/*.txt $(WINE32)/*.reg $(WINE32)/wcc $(WINE32)/Makefile $(WINE32)/legacy32/*.sh $(WINE32)/legacy32/*.py $(WINE32)/legacy32/*.txt $(WINE32)/legacy32/*.c $(WINE32)/legacy32/*.mk)),8)
# WINE32_TLS=0: the loader without GnuTLS (crypt32's and schannel's dlopen of it
# then fails, as on a host without it), 1.8 MB less wasm, for a site that
# never speaks https. The Wine build is the same either way.
WINE32_TLS ?= 1
export WINE32_TLS
# WINE32_MONO=0: no Wine Mono in the dist (.NET programs then fail to start,
# as on a host without it), 210 MB less.
WINE32_MONO ?= 1
export WINE32_MONO
# WINE_WIN16=1: the dist keeps Win16 and VxD (the 16-bit modules, winevdm,
# wow32, w32skrnl), which it otherwise leaves out (wine32/stage.sh).
WINE_WIN16 ?= 0
export WINE_WIN16
# WINE_ICU=1: the dist keeps icu (and icuin, icuuc), which it otherwise leaves out.
WINE_ICU ?= 0
export WINE_ICU
# WINE_GSTREAMER=1: the dist keeps winegstreamer, which it otherwise leaves out.
WINE_GSTREAMER ?= 0
export WINE_GSTREAMER
# A release dist's PE modules (i386-windows/*.wasm) are without the names gex
# never reads (and producers, debug info): gexcc-win's stripnames.py, as
# wine64-dist.sh's are. The key gets -s<its hash>. WINE32_STRIP=0 keeps the
# names, for a debug dist (-dbg).
WINE32_STRIP ?= 1
STRIPNAMES := $(WINE32)/../toolchain/gexcc/windows/stripnames.py
STRIP_KEY := $(if $(filter 0,$(WINE32_STRIP)),-dbg,-s$(shell sha256sum < $(STRIPNAMES) | cut -c1-6))
DIST_KEY := $(WINE_ID)-$(DEPS_KEY)-$(GL_KEY)-$(TOOLS_KEY)$(if $(filter 0,$(WINE32_TLS)),-notls)$(if $(filter 0,$(WINE32_MONO)),,-mono$(WINE_MONO_VERSION))$(if $(filter 1,$(WINE_WIN16)),,-no16)$(if $(filter 1,$(WINE_ICU)),,-noicu)$(if $(filter 1,$(WINE_GSTREAMER)),,-nogst)$(STRIP_KEY)
DIST := $(C)/dist-$(DIST_KEY)
BARE := $(C)/pfx-bare-$(DIST_KEY)
INSTALLED := $(C)/pfx-installed-$(DIST_KEY)
PREFIX := $(C)/pfx-$(DIST_KEY)-wayland
PREFIX_NULL := $(C)/pfx-$(DIST_KEY)-null

.PHONY: all dist prefix prefix-null
all: dist prefix
dist: $(DIST)/.done
prefix: $(PREFIX)/.done
prefix-null: $(PREFIX_NULL)/.done

# The dist: the loader, the server, the staged PE side and Wine's raster
# fonts (wine-fon.sh; the tools build, without FreeType, makes none).
$(DIST)/.done: $(BUILD)/.done-$(WINE_ID) $(GL_O)
	rm -rf $(DIST).tmp $(BARE).tmp
	$(NICE) $(WINE32)/legacy32/link-loader.sh $(BUILD) $(WINE_SRC) $(BUILD)/gexwine32-obj $(DEPS) $(GL_O) $(DIST).tmp
	JOBS=$(WINE32_JOBS) $(NICE) $(WINE32)/link-wineserver.sh $(BUILD) $(WINE_SRC) $(BUILD)/gexwine32-server $(DIST).tmp
	$(NICE) $(WINE32)/legacy32/stage.sh $(BUILD) $(WINE_SRC) $(DIST).tmp $(BARE).tmp
	rm -rf $(DIST).fon && JOBS=$(WINE32_JOBS) $(NICE) $(WINE32)/wine-fon.sh $(WINE_SRC) $(TOOLS)/include $(C)/dl $(DIST).fon
	mv $(DIST).fon/*.fon $(DIST).tmp/share/wine/fonts/ && rmdir $(DIST).fon
	$(if $(filter 0,$(WINE32_MONO)),,$(WINE32)/legacy32/mono.sh $(C)/dl $(DIST).tmp)
	$(if $(filter 0,$(WINE32_STRIP)),,find $(DIST).tmp/lib/wine/i386-windows -maxdepth 1 -type f -name '*.wasm' -print0 | \
	  xargs -0 -r -P$(WINE32_JOBS) -n 16 sh -c 'for f; do python3 "$$0" "$$f" "$$f.s" && mv "$$f.s" "$$f" || exit 1; done' $(STRIPNAMES))
	rm -rf $(DIST) $(BARE) && mv $(BARE).tmp $(BARE) && mv $(DIST).tmp $(DIST)
	@echo "$(DIST_KEY) wine=$(WINE_ID) musl=$(MUSL_COMMIT) src=$(WINE_SRC)" > $(DIST)/.wine32-build
	touch $@

$(INSTALLED)/.done: $(DIST)/.done
	@[ -x $(GEXC) ] || { echo "no x86 gexc at $(GEXC) (cargo build --release -p gexc --features x86, or GEXC=)"; exit 1; }
	$(NICE) $(WINE32)/legacy32/prefix.sh install $(BARE) $(DIST) $(GEXC) $(INSTALLED)
	touch $@

$(PREFIX)/.done: $(INSTALLED)/.done
	$(WINE32)/legacy32/prefix.sh ready $(INSTALLED) $(DIST) wayland $(PREFIX)
	touch $@

$(PREFIX_NULL)/.done: $(INSTALLED)/.done
	$(WINE32)/legacy32/prefix.sh ready $(INSTALLED) $(DIST) null $(PREFIX_NULL)
	touch $@
