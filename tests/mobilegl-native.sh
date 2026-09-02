#!/bin/sh
# Build and run MobileGL's semantic frontend tests without LTO. The fork's
# mismatched vendored SPIR-V headers violate ODR under GCC LTO and crash during
# gtest discovery; Debug/no forced optimization avoids masking test results
# behind that independent link defect.
set -eu

ROOT="$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)"
BUILD="${GEX_MOBILEGL_NATIVE_BUILD:-${TMPDIR:-/tmp}/gex-mobilegl-native-test}"
MOBILEGL="$ROOT/third_party/mobilegl-webgpu"

cmake -S "$MOBILEGL" -B "$BUILD" -G Ninja \
    -DCMAKE_BUILD_TYPE=Debug \
    -DMOBILEGL_FORCE_RELEASE_OPT=OFF \
    -DMOBILEGL_ENABLE_LTO=OFF \
    -DMOBILEGL_BUILD_TEST=ON \
    -DMOBILEGL_BUILD_BENCHMARK=OFF \
    -DMOBILEGL_BUILD_TRACE_REPLAY=OFF \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5
cmake --build "$BUILD" --parallel "${JOBS:-8}"
ctest --test-dir "$BUILD/MobileGL/MG_Test" --output-on-failure
