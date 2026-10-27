# wine-wasm

Wine for gex. Wine's own Unix side (the loader, ntdll's Unix half, the
wineserver and the drivers) is built as wasm32 with gexcc. The Windows code
it loads is x86, and gex translates it like any other guest code. Builds are
reproducible from the pins in `wine32.pins`.

It needs an x86 `gexc` at `target/x86/release/gexc`
(`cargo build --release -p gexc --features x86 --target-dir target/x86`),
or `GEXC=` naming one.

```sh
make -C tools/wine-wasm              # the Wine install and a ready-to-run prefix
make -C tools/wine-wasm prefix-null  # a prefix with the null display driver
make -C tools/wine-wasm paths        # where the outputs are
tools/wine-wasm/run.sh app.exe       # run a Windows program
```

Outputs go to a cache directory named after their inputs, so a rerun only
rebuilds what changed. The `Makefile` header lists the settings.

64-bit Wine is in progress. It is built from the Wine and musl commits in
`wine64.pins`, beside a 32-bit Wine build tree (`make build`, made first when
missing):

```sh
tools/wine-wasm/build-sysroot64.sh SR    # the LP64 sysroot; fetches the pinned musl
git clone WINE64_REPO wine64 && git -C wine64 checkout WINE64_COMMIT
GEX_WASM64_SYSROOT=SR/sysroot tools/wine-wasm/wine64-dist.sh wine64 CACHE
```

`wine64-dist.sh` prints where the install and its prefix are.
