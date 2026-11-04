# llvm_pin_check DIR: DIR is a clean checkout of toolchain/pins' LLVM_COMMIT, else exit 1
# (sourced by build-cxx.sh and build-cxx-win.sh)
llvm_pin_check() {
  _pin=$(sed -n 's/^LLVM_COMMIT *:= *//p' "$(dirname "$0")/../pins")
  [ -n "$_pin" ] || { echo "no LLVM_COMMIT in toolchain/pins" >&2; exit 1; }
  _at=$(git -C "$1" rev-parse HEAD 2>/dev/null) || { echo "$1: not a git checkout (toolchain/pins: $_pin)" >&2; exit 1; }
  [ "$_at" = "$_pin" ] || { echo "$1 is at $_at, not toolchain/pins' $_pin" >&2; exit 1; }
  [ -z "$(git -C "$1" status --porcelain --untracked-files=no)" ] || { echo "$1 has local changes (toolchain/pins: $_pin)" >&2; exit 1; }
}
