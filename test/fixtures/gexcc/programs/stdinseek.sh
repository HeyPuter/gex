# run as: sh stdinseek.sh PROG
printf 'abcdefghij\n' > stdinseek.in
(exec 3< stdinseek.in; "$1" <&3; echo "next: $(head -c 4 <&3)")
# a pipe: the program takes only what it reads (unbuffered)
printf 'abcdefghij\n' | { "$1" > /dev/null; echo "after: $(cat)"; }
printf 'line\n' > stdinseek.out; "$1" < stdinseek.in >> stdinseek.out; cat stdinseek.out
rm -f stdinseek.in stdinseek.out
