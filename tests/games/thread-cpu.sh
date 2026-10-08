#!/bin/sh
# thread-cpu.sh PID SECS: each thread's CPU use over SECS (percent of one core), busiest first,
# from /proc/PID/task/*/stat (utime+stime, clock ticks). Thread names are the host's (gexc names a
# guest thread's host thread `gex-tid-N`, N the guest tid).
pid=$1 secs=$2
hz=$(getconf CLK_TCK)
snap() { for t in /proc/$pid/task/*; do [ -r $t/stat ] && echo "${t##*/} $(cut -d')' -f2 < $t/stat | awk '{print $12+$13}') $(cat $t/comm 2>/dev/null | tr ' ' _)"; done; }
a=$(snap); sleep "$secs"; b=$(snap)
echo "$a" > /tmp/tcpu.a.$$; echo "$b" > /tmp/tcpu.b.$$
awk -v hz="$hz" -v s="$secs" 'NR==FNR {t[$1]=$2; next} ($1 in t) {d=$2-t[$1]; if (d>0) printf "%6.1f%% %s %s\n", 100*d/hz/s, $1, $3}' /tmp/tcpu.a.$$ /tmp/tcpu.b.$$ | sort -rn | head -20
awk -v hz="$hz" -v s="$secs" 'NR==FNR {t[$1]=$2; next} ($1 in t) {d+=$2-t[$1]} END {printf "total %.1f%% of one core\n", 100*d/hz/s}' /tmp/tcpu.a.$$ /tmp/tcpu.b.$$
rm -f /tmp/tcpu.a.$$ /tmp/tcpu.b.$$
