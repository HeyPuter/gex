# Part of tools/x86-difftrace (run.sh).
# cmp.py A B: first line where two traces differ (pc, then registers, then
# flags). Registers that differ from the first line on (the harness's own
# esi/edi/ebp) are ignored until either trace writes them.
import sys
def dedup(f):
    """A rep-prefixed instruction single-steps once per element natively;
    gex reports it once: keep the first of a run at one pc."""
    last = None
    for line in f:
        pc = line.split(" ", 1)[0]
        if pc != last:
            yield line
        last = pc
a, b = dedup(open(sys.argv[1])), dedup(open(sys.argv[2]))
names = ["pc", "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "fl"]
ignore = None
prev = None
for n, (x, y) in enumerate(zip(a, b)):
    x, y = x.split(), y.split()
    if ignore is None:
        ignore = {i for i in range(1, 9) if x[i] != y[i]}
        base = (list(x), list(y))
    for i in list(ignore):
        if x[i] != base[0][i] or y[i] != base[1][i]:
            ignore.discard(i)
    diff = [i for i in range(10) if x[i] != y[i] and i not in ignore]
    if diff:
        print("line", n, "differs in", [names[i] for i in diff])
        print("prev", prev)
        print("A   ", " ".join(x))
        print("B   ", " ".join(y))
        break
    prev = " ".join(x)
else:
    print("no difference in", n + 1, "lines")
