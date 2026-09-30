/* A forwarding file, kept only for the path. The decoder is
 * web/mesa/gexgl-mesa.cpp.
 *
 * The pinned Mesa fork (third_party/mesa) compiles the decoder by this path
 * in src/gallium/targets/gex-wasm/meson.build. Delete this file once the
 * fork's meson.build names web/mesa/gexgl-mesa.cpp and the submodule points
 * at that commit. */
#include "../mesa/gexgl-mesa.cpp"
