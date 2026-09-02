# Local changes to third-party submodules

`third_party/mesa` has a home of its own: it points at
[mesa-wink](https://github.com/MercuryWorkshop/mesa-wink), branch `gex-wgpu`,
so the `gex_wgpu` driver is committed history rather than a patch. Nothing in
this directory concerns it.

The other two submodules point at repositories nobody here can push to --
Khronos's and r58Playz's -- so their local changes are kept as patches
instead, and would otherwise exist only in one working tree.

| Patch | Against | What it is |
| --- | --- | --- |
| `vk-gl-cts.patch` | KhronosGroup/VK-GL-CTS | The `targets/gex` build target and a surfaceless-platform fix, which is what makes KHR-GL33 buildable for the guest. Needed to reproduce any CTS number in `docs/`. |
| `mobilegl-webgpu.patch` | r58Playz/MobileGL | The abandoned MobileGL WebGPU backend. Kept only so the work is not lost; **do not build or run it** -- it is gated off deliberately and is not a useful comparison against the Mesa path. |

Regenerate after changing a submodule:

    cd third_party/<name> && git add -N . && git diff > ../patches/<name>.patch

Apply to a fresh checkout:

    cd third_party/<name> && git apply ../patches/<name>.patch
