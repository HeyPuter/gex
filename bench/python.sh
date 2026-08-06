#!/bin/sh
# CPython under gex, against qemu.
#
#   bench/python.sh <debian-rootfs>
#
# Worth having as its own benchmark for two reasons. It is the hardest shape
# a binary translator meets -- a bytecode interpreter is one enormous
# function of indirect dispatch, which is exactly what a block-per-function
# translation is worst at -- and Debian's python3-minimal links the
# interpreter into the executable, so every instruction that matters is
# translated ahead of time. There is no runtime translation in these numbers
# at all. It is a measurement of the static recompiler and nothing else.
#
# Measured on this machine, medians, gex against qemu-user, over three
# separate invocations of this script:
#
#   fannkuch (integer, list)    1.04  1.08  0.99
#   nbody    (float)            0.68  0.66  0.69
#   dicts    (allocation)       1.03  1.08  1.08
#
# So: at parity on integer and list work, five to eight per cent behind on
# allocation, and about a third faster on floating point. All three produce
# results identical to the host's python, to the last digit of a float and
# the last of a 1234-digit integer.
#
# One `Segmentation fault` was seen in about twenty runs of this and did not
# recur in twenty more, and which engine produced it was not established. The
# suite has an intermittent qemu=139 under load with the same shape. Recorded
# rather than explained.
#
# Regions were the obvious thing to try here, since a dispatch loop is a loop
# and regions exist to keep one in a single wasm function. Paired medians of
# five, alternating which configuration ran first:
#
#   fannkuch 1.010    nbody 0.968    dicts 0.961
#
# Neutral to four per cent, which is the same answer the other workloads gave
# (see crates/gex-emit/src/region.rs). The interpreter loop is not one loop as
# far as discovery is concerned; it is a `br` through a jump table into a few
# hundred separate handlers, and a region cannot hold that.
set -eu

R="${1:?usage: bench/python.sh <debian-rootfs>}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
GEXC="$HERE/target/release/gexc"
QEMU="${GEX_QEMU:-$HOME/toolchains/qemu-aarch64}"
PY=/usr/bin/python3.11
ROUNDS="${ROUNDS:-5}"

[ -x "$GEXC" ] || { echo "skip: build gexc first"; exit 0; }
[ -x "$R$PY" ] || { echo "skip: no $PY in $R -- run apt-get install python3-minimal"; exit 0; }

cat > "$R/var/bench.py" <<'PYEOF'
# Three shapes: integer and list, floating point, and allocation.
import time


def fannkuch(n):
    p, q, s = list(range(n)), list(range(n)), list(range(n))
    sign, maxflips, total = 1, 0, 0
    while True:
        q0 = p[0]
        if q0 != 0:
            for i in range(1, n):
                q[i] = p[i]
            flips = 1
            while True:
                qq = q[q0]
                if qq == 0:
                    total += sign * flips
                    if flips > maxflips:
                        maxflips = flips
                    break
                q[q0] = q0
                q0 = qq
                flips += 1
                i, j = 1, q0 - 1
                while i < j:
                    q[i], q[j] = q[j], q[i]
                    i += 1
                    j -= 1
        if sign == 1:
            p[0], p[1] = p[1], p[0]
            sign = -1
        else:
            p[1], p[2] = p[2], p[1]
            sign = 1
            for i in range(2, n):
                if s[i] != 0:
                    s[i] -= 1
                    break
                if i == n - 1:
                    return maxflips, total
                s[i] = i
                p = p[1:i + 2] + [p[0]] + p[i + 2:]


def nbody(steps):
    bodies = [[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 39.5],
              [4.84, -1.16, -0.10, 0.606, 2.81, -0.025, 0.0119],
              [8.34, 4.12, -0.40, -1.01, 1.82, 0.0084, 0.00356]]
    dt = 0.01
    for _ in range(steps):
        for i, a in enumerate(bodies):
            for b in bodies[i + 1:]:
                dx, dy, dz = a[0] - b[0], a[1] - b[1], a[2] - b[2]
                d = (dx * dx + dy * dy + dz * dz) ** 0.5
                mag = dt / (d * d * d) if d else 0.0
                a[3] -= dx * b[6] * mag
                a[4] -= dy * b[6] * mag
                a[5] -= dz * b[6] * mag
                b[3] += dx * a[6] * mag
                b[4] += dy * a[6] * mag
                b[5] += dz * a[6] * mag
        for a in bodies:
            a[0] += dt * a[3]
            a[1] += dt * a[4]
            a[2] += dt * a[5]
    return sum(a[0] + a[1] + a[2] for a in bodies)


def dicts(n):
    d = {}
    for i in range(n):
        d[str(i)] = i * i
    return sum(v for k, v in d.items() if len(k) == 4)


for name, fn, arg in (("fannkuch", fannkuch, 8), ("nbody", nbody, 30000),
                      ("dicts", dicts, 120000)):
    t = time.time()
    r = fn(arg)
    print("%s %.3f %s" % (name, time.time() - t, r))
PYEOF

# The guest times itself, so translation and startup are outside the number.
# A cold run first, because the first one pays for Cranelift.
"$GEXC" run --uid 0 --sysroot "$R" "$R$PY" /var/bench.py >/dev/null 2>&1 || true

i=0
while [ "$i" -lt "$ROUNDS" ]; do
    i=$((i + 1))
    # Alternate which engine goes first, so a machine that warms up or throttles
    # does not favour one of them.
    if [ $((i % 2)) -eq 0 ]; then
        "$GEXC" run --uid 0 --sysroot "$R" "$R$PY" /var/bench.py | sed 's/^/gex /'
        "$QEMU" -L "$R" "$R$PY" /var/bench.py | sed 's/^/qemu /'
    else
        "$QEMU" -L "$R" "$R$PY" /var/bench.py | sed 's/^/qemu /'
        "$GEXC" run --uid 0 --sysroot "$R" "$R$PY" /var/bench.py | sed 's/^/gex /'
    fi
done > "$R/var/bench.out"

awk '
{ t[$1 "," $2] = t[$1 "," $2] " " $3; answer[$2] = answer[$2] " " $4 }
END {
    split("fannkuch nbody dicts", names, " ")
    for (k = 1; k <= 3; k++) {
        n = names[k]
        printf "%-9s gex %6.3fs   qemu %6.3fs   ratio %5.3f\n", n,
               med(t["gex," n]), med(t["qemu," n]),
               med(t["gex," n]) / med(t["qemu," n])
    }
}
function med(s,   a, c, i) {
    c = split(s, a, " ")
    for (i = 1; i < c; i++) for (j = i + 1; j <= c; j++)
        if (a[j] + 0 < a[i] + 0) { x = a[i]; a[i] = a[j]; a[j] = x }
    return a[int((c + 1) / 2)] + 0
}
' "$R/var/bench.out"

# Every run must agree with every other, whichever engine produced it. A
# faster wrong answer is not a result.
if [ "$(awk '{print $2, $4}' "$R/var/bench.out" | sort -u | wc -l)" -ne 3 ]; then
    echo "MISMATCH: the two engines disagree"
    awk '{print $1, $2, $4}' "$R/var/bench.out" | sort -u
    exit 1
fi
echo "all runs agree"
