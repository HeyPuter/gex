// Which build's code a bundle carries: the default one (SharedArrayBuffer, a
// Worker per guest thread) and the single-thread one (single.mjs: no
// SharedArrayBuffer, every thread a fiber of one Worker). The source tree
// carries both and picks at run time. Each package carries one:
// page/runtime/build.sh puts a module of its own in this one's place, SP_CODE
// false for @gexemu/runtime and MP_CODE false for @gexemu/runtime-single, so the
// bundler drops the other's code. A test of the mode tests the constant
// first, as `SP_CODE && single.on`.
export const SP_CODE = true;
export const MP_CODE = true;
