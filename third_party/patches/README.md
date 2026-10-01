# Local changes to third-party submodules

`third_party/mesa` has a home of its own: it points at
[mesa-wink](https://github.com/MercuryWorkshop/mesa-wink), branch `gex-wgpu`,
so the `gex_wgpu` driver is committed history rather than a patch. Nothing in
this directory concerns it.

`third_party/vk-gl-cts` points at a repository nobody here can push to
(Khronos's), so its local changes are kept as a patch instead, and would
otherwise exist only in one working tree.

| Patch | Against | What it is |
| --- | --- | --- |
| `vk-gl-cts.patch` | KhronosGroup/VK-GL-CTS | The `targets/gex` build target and a surfaceless-platform fix, which is what makes KHR-GL33 buildable for the guest. Needed to reproduce any CTS number in `docs/`. |

Regenerate after changing a submodule:

    cd third_party/<name> && git add -N . && git diff > ../patches/<name>.patch

Apply to a fresh checkout:

    cd third_party/<name> && git apply ../patches/<name>.patch
