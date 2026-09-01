#!/bin/sh
# Run a GL program against the natively-built gex_wgpu. See
# web/build-mesa-native.sh.
set -u
INST=${GEX_MESA_NATIVE_OUT:-/tmp/gex-mesa-native}/inst
DAWN=${GEX_DAWN_PKG:-$HOME/.cache/gex-dawn-native/pkg}
export DISPLAY=${GEX_DISPLAY:-:77}
export LD_LIBRARY_PATH=$INST/lib:$DAWN/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export LIBGL_DRIVERS_PATH=$INST/lib/dri
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
exec "$@"
