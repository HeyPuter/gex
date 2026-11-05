# test/page/wasm32-page: a wasm32 busybox as the page's /bin/sh and coreutils runs this
# (pipelines, command substitution, subshells, redirections, externals, a failing
# command: hush forks for them) against the host's sh, both in a directory holding
# in.txt (b, a, c, b).
echo b a | tr " " "\n" | sort | head -1
cat in.txt | sort | uniq | wc -l
x=$(grep -c b in.txt); echo "b lines: $x"
sed s/b/B/ in.txt | grep -c B
mkdir -p d/e && cp in.txt d/e/ && ls d/e && rm -rf d
for i in 1 2 3; do echo $i; done | tail -1
awk "{n++} END {print n}" in.txt
false || echo "false failed"
/nonexistent-command 2>/dev/null; echo "status $?"
(cd / && pwd); x=1; (x=2; echo "sub $x"); echo "top $x"
{ echo a; echo b; } | sort -r | tr "\n" " "; echo
echo $(echo $(echo nested))
f() { echo "in f $1"; }; f one | cat
echo done
