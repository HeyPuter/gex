#!/bin/bash
# Run a GL program against the natively-built gex_wgpu.
set -u
INST=/tmp/gex-mesa-native/inst
export DISPLAY=${GEX_DISPLAY:-:77}
export LD_LIBRARY_PATH=$INST/lib:$HOME/.cache/gex-dawn-native/pkg/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export LIBGL_DRIVERS_PATH=$INST/lib/dri
export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=${GALLIUM_DRIVER:-gex_wgpu}
exec "$@"
