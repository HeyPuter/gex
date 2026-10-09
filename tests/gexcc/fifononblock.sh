rm -f fifononblock.p; mkfifo fifononblock.p
timeout 10 "$1" fifononblock.p; echo "status $?"
rm -f fifononblock.p
