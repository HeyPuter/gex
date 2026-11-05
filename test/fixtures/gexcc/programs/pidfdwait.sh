# pidfdwait under a time limit: a freeze (every process parked) fails
# rather than hanging the run
timeout -k 5 120 "$1"; s=$?
[ $s = 124 ] && echo "froze: no answer in 120 s"
exit $s
