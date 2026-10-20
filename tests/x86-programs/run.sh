#!/bin/sh
# x86 guest programs whose behaviour is checked against native execution:
# each is built static for x86-64 and i386, run natively and under gexc, and
# its output and exit status must match.
#
#   tests/x86-programs/run.sh [gexc]      (default target/x86/release/gexc)
#
# Needs an x86-64 host and zig (for the musl targets). The `page` tier runs the
# x86-64 build under the browser shim in Node (run-node.mjs, memory64 shared
# seed) for the programs listed in PAGE_PROGRAMS, when that seed has been built
# (GEX_ARCH=x86 GEX_MEM64=1 shims/browser/build.sh) and node is on PATH.
#
# TIERS limits the run to those tiers (a space-separated list of t1, fjit,
# page; all by default): a job that only wants the page, beside another that
# runs the rest, sets TIERS=page.
set -u
# Output is compared with the native run: not even gex's default lines (log.mjs).
export GEX_LOG=${GEX_LOG:-off}
TIERS=${TIERS:-}
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
fail=0
SEED64=web/public/gex/seed.x86.m64.shared.wasm
# tgkill-futex: a signal sent to a thread parked on a futex must end its wait
# (Boehm's stop-the-world; the page asked the wrong thread's signal state).
# exec-tail: code ending in the last bytes of an executable mapping (the page
# fetched only up to four bytes before the end: SIGILL in Mono's JIT code).
# retire-during-lift: code retired while the translator lifts other code
# outside the kernel lock (docs/kernel-locking.md step 5). Tier-1 only: the
# function JIT livelocks on its chain natively (gexc --fjit, 2026-10-12).
# close-while-reading: descriptors read without the kernel lock while other
# threads close and replace them; pipe and socket stress (step 3).
# thread-signals, signal-storm: signals asked about and delivered by each
# thread itself at its safepoints, without the kernel lock (step 8): masks
# and targets per thread, a process-directed signal taken exactly once.
# exec-signals: the mask, ignored signals and pending ones survive execve.
# mem64-wx: a W^X JIT's high pages made executable (mprotect, and a fixed
# executable mapping at 11 GiB): the M64 dense directory (refused before).
# mem64-console: console output from above 4 GiB (a thread's stack, a
# mapping); the address used to be cut to 32 bits.
# clock-monotonic: the page's CLOCK_MONOTONIC never goes back, within a thread
# or between two Workers, and is finer than a millisecond.
# pwait-sigmask: epoll_pwait, ppoll and pselect6 wait under their sigmask
# argument and put the old mask back (gex ignored it: foot never took
# SIGTERM); i386 also checks the legacy frame's saved mask.
# main-thread-exit, pi-mutex, tcp-loopback: the process outlives its first
# thread; a contended PI mutex; a forked child's loopback TCP to its parent.
# sa-restart: a handler installed with SA_RESTART restarts wait4, read and
# an untimed futex wait; poll and nanosleep still say EINTR (gex said EINTR
# for all of them); i386 also checks setitimer's microseconds.
# thread-churn: more threads over a process's life than the page has Worker
# slots, and a fork child starting threads while its parent holds 40.
# page-cross-entry: a function entered at a page's last byte, its next page
# retired; in the page with GEX_TEST_UNPUBLISHED_RETIRE, which leaves a
# retired page numbered with no directory entry (a publisher that never
# installed), the block's page check must converge (Paint's startup stall).
# stale-cross-page: a block across a page boundary whose second page is
# rewritten 150 times (past the stale check's bound of 64); the bound once
# counted for the whole run and stranded the block (FL's ACM codecs).
# stale-loop-head: a loop whose head page is rewritten while its body's page
# is not; the body's old translation jumps straight to the old head, whose
# page check fails, and the page must resume through dispatch rather than
# translate the head again each iteration (FL Studio's page startup: 34k
# translations, then the renderer out of memory).
# kernel-contend: threads in the kernel at once (docs/kernel-locking.md
# step L1 on): a contended mutex, condvar ping-pong, PI locks, mmap/munmap
# churn and identity calls from four threads, counts exact; each phase's time
# goes to stderr, KERNEL_CONTEND_SCALE=N multiplies the work (the microbench).
# sleep-threads: nanosleep and clock_nanosleep from several threads, absolute
# and past deadlines, EINVAL, and a signal ending a sleep (step L2 on).
# sendmsg-rights, scm-fork: socket messages and descriptor passing, within a
# process and to a forked child (step L5 on: sendmsg/recvmsg without the lock).
# exec-futex: futexes in an exec'd image (the lock-free futex path's view of
# the memory map must be the new program's).
# spawn-threads: posix_spawn from a process of four threads, which the page
# runs in the calling thread's Worker (shims/browser/src/vfork.rs): the
# child's descriptors and signal state its own, the exec and its failure,
# the other threads running on. Page only: natively gexc's posix_spawn says
# ENOENT (it did before the page's vfork too).
# served-entries: two programs at one base (it and its VAL=2 build, ARGV0-b,
# which it execs) sharing a file of code; in the page with
# GEX_TEST_SERVE_UNMERGED (served modules installed one at a time) the second
# must not be pointed at the first's code on a page they both have.
# fdtable-threads: dup, dup2, dup3, fcntl, flock and close racing on one
# descriptor table without the kernel lock (L10); a flock goes with its
# description's last close, not before.
# mmap-threads: mmap, munmap, mprotect, mremap, madvise and msync racing on
# one map without the kernel lock (L12b), code mapped where another thread
# just unmapped code run as the new code.
# codemap-threads: copies of a file's code mapped and called from several
# threads while others map anonymous memory: a miss in file-backed code
# (note_intrinsics) took the JIT lock under the map's, against the lock
# order (L13). The drains after each mapping (providers, units, the
# recompile queue) run without the kernel lock (L12c).
# proc-threads: /proc/self read while other threads map, unmap, open and
# close; cmdline in a forked child and after execve; a child's stat (L14a:
# /proc reads the process's published state, no refresh under the lock).
# fs-threads: a chdir in one pthread seen by its siblings; unshare(CLONE_FS)
# gives a thread its own directory, shared with the threads it makes and
# kept by its execve; *at calls through a directory descriptor racing
# renames of files and directories in it and of its own directory, which it
# follows (L14b, L14c: the path calls without the kernel lock).
# sig-state-threads: sigaction, sigaltstack, sigpending and getrandom from
# several threads while an interval timer fires (L15: the calls on a
# process's and thread's own signal state without the kernel lock).
# lift-threads: four threads translating fresh chains of code at once and
# unmapping them (retiring) while two others make kernel calls (L16: the
# miss's lift and install without the kernel lock).
# fdmisc-threads: pipes, one epoll set, ftruncate, rlimits, statfs and
# sysinfo from several threads at once (L17: the small descriptor and
# process calls without the kernel lock).
# task-ids: thread and process ids are one numbering; kill, tgkill and
# /proc/<tid> agree on which task an id is (L15b).
# sock-threads: a unix listener, its clients, socketpairs and internet
# sockets from several threads at once (L18: the socket calls without the
# kernel lock).
# exec-dethread: an exec from a second thread while the first joins, spins,
# reads or waits on a futex; the old program must not run over the new one
# (the page's de_thread). Page only, as spawn-threads.
# fork-maps-threads: forks while other threads map, protect and unmap memory
# and files (L19: a fork's image, code names and the translator's file reads
# come from the published map, not the kernel lock); the child's image and
# memory cover every mapping, thread stacks above 4 GiB too.
# fork-thread-ctx: a process forked or exec'd from a thread other than the
# first starts threads; none may share the main thread's context.
# fork-twice-thread: a shell forked from a second thread launches two
# children in quick succession from a thread of its own; all keep running.
# quiet-threads: uname, the scheduling calls, getcpu, fsync, getgroups,
# getppid, arch_prctl, times and time from several threads at once while
# children are forked (L22: the calls that need nothing of the process,
# without the kernel lock).
# fdobj-threads: eventfds ping-ponged between blocking readers, timerfds,
# epoll sets, memfds grown with fallocate and mapped, and a splice that
# blocks on an empty pipe, then sendfile and copy_file_range, from several
# threads at once (L22b: the calls that make or work a descriptor's object,
# without the kernel lock).
# cred-threads: one thread changes the process's uid and gid while two
# others check in turn that getuid, getresuid, a new file's owner and the
# sender of a signal they send themselves all show the new ids, and another
# asks getpgid and getsid (L22c: the credential and session calls without
# the kernel lock; the ids every thread reads are the process's, now).
# kill-threads: a token passed round four threads by tgkill, a read
# interrupted by one, sigsuspend woken by one twenty times, the process's
# own kill taken by sigtimedwait, and a child ended by kill (L22d: the
# signal calls without the kernel lock).
# sigwait-threads: sigwaitinfo and sigtimedwait that wait, taking SIGUSR1s
# sent by tgkill, by kill to two waiters, and by a child; a timeout that
# says EAGAIN after its time, and a wait a handled signal interrupts.
# tid-threads: pthreads that name a word of their own with set_tid_address
# while others are made and joined; each named word is cleared and woken by
# its thread's exit, the rest are left alone (L22e: the calls on a thread's
# own state without the kernel lock).
# mount-threads: four threads mount, fill and unmount tmpfs's of their own
# while one thread's mount is filled and unmounted by another (L22e: mount
# and umount2 without the kernel lock, through the namespace's published
# view). Natively it needs an unprivileged user namespace (unshare -rm).
# exit-threads: a forked child's process ended by exit_group from its main
# thread, from another thread, from two at once, and by its last thread's
# exit, while its other threads are parked in a read, an accept and a futex
# wait; its descriptors are closed before its status is seen (L24: process
# end without the kernel lock).
# fork-maps-threads (native tiers only for now): forks while other threads
# map, protect and unmap memory and files (L19: a fork's image, code names
# and the translator's file reads come from the published map, not the
# kernel lock). Not in the page tier: in the memory64 page a forked child
# of a threaded parent can fault walking musl's thread list, whose
# descriptors sit on thread stacks above 4 GiB that its memory was not grown
# to cover (fails the same on 31a21cad).
# many-workers: three children of 48 threads each, all parked at once: more
# Workers than the global kernel had stacks for (128; the Win7 shell's
# Games folder said "this is Worker 129").
# poll-timeout: poll, ppoll, select and epoll_wait ended only by their
# 100 ms deadline, after the stack went back to the page (the clock scope a
# trap left open froze the clock, and they never returned).
# fork-dirs: a forked child running its parent's code on pages the parent
# numbered; any "gex: heal" line in the page's stderr fails it (heal_dirs
# writing a directory entry is a bug report).
# xattr: user.* extended attributes (Wine's DOS attributes), in the page's
# shared tmpfs (the global kernel's) as natively in the host's.
# mmap-coherent: MAP_SHARED mappings agree with read/write of the file, and
# a second open of it maps the same pages (KC, scratch/KC/map-shared.md A).
# mmap-writeback: a fork's child writes its pages of a shared mapping back
# at exit, and only those (B; the page carries the mapping to the child's
# Worker with its descriptors).
# sig-regs-threads: SIGUSR1 sent to spinning threads (a back edge, an
# indirect jump, a call, a system call, a loop across a page) reports every
# live register in the handler's ucontext, at whatever safepoint it lands
# (Wine's suspend sends the frame's context to the server and resumes from
# it). x86-64 only (its loops are assembly); under the function JIT too.
# null-access (with GEX_NULL_GUARD=1, the debug opt-in): loads and stores
# through null pointers (and below 64 KiB) fault: a SIGSEGV handler runs and
# jumps back out, and with no handler the process dies of SIGSEGV.
PAGE_PROGRAMS=${PAGE_PROGRAMS:-"fork-dirs spawn-threads served-entries page-cross-entry tgkill-futex exec-tail mem64-io mem64-console mem64-smc mem64-reserve retire-during-lift close-while-reading thread-signals signal-storm exec-signals clock-monotonic main-thread-exit pi-mutex tcp-loopback pwait-sigmask sa-restart thread-churn kernel-contend exec-futex sleep-threads sendmsg-rights scm-fork ioctl-threads fdtable-threads mmap-threads codemap-threads proc-threads fs-threads sig-state-threads lift-threads fdmisc-threads task-ids sock-threads exec-dethread fork-thread-ctx fork-maps-threads fork-twice-thread quiet-threads fdobj-threads cred-threads kill-threads sigwait-threads poll-timeout tid-threads mount-threads exit-threads many-workers stale-loop-head stale-cross-page xattr mem64-wx mmap-coherent mmap-writeback null-access sig-regs-threads"}
for src in "$HERE"/*.c; do
    name=$(basename "$src" .c)
    # mem64-*: a 64-bit guest memory (docs/mem64.md), x86-64 only.
    gexflags=""
    targets="x86_64-linux-musl x86-linux-musl"
    case $name in mem64-*) gexflags=--mem64; targets=x86_64-linux-musl ;; esac
    case $name in sig-regs-threads) targets=x86_64-linux-musl ;; esac
    # *smc*: self-modifying code, under the function JIT as well (which
    # must leave writable-and-executable code to tier-1's guards).
    tiers="t1"
    case $name in *smc* | copy-loop | null-access | sig-regs-threads) tiers="t1 fjit" ;; esac
    case " $PAGE_PROGRAMS " in *" $name "*)
        if [ -f "$SEED64" ] && command -v node >/dev/null; then tiers="$tiers page"; fi ;;
    esac
    case $name in spawn-threads) tiers=${tiers#t1} ;; esac
    for target in $targets; do
    for tier in $tiers; do
        [ $tier = page ] && [ "$target" != x86_64-linux-musl ] && continue
        case " ${TIERS:-$tier} " in *" $tier "*) ;; *) continue ;; esac
        flags=$gexflags
        [ $tier = fjit ] && flags="$flags --fjit"
        extra=""
        [ "$target" = x86-linux-musl ] && extra="-Wl,--image-base=0x1000000"
        bin="$WORK/$name-$target"
        [ -x "$bin" ] || zig cc -target "$target" -O1 -static $extra -o "$bin" "$src" || { fail=1; continue; }
        if [ $name = served-entries ]; then
            [ -x "$bin-b" ] || zig cc -target "$target" -O1 -static $extra -DVAL=2 -o "$bin-b" "$src" || { fail=1; continue; }
        fi
        native=$("$bin" 2>/dev/null); ns=$?
        # smc-race: the translator waits between decoding a JIT-code block
        # and fixing its guard, so the writer's stores land in between.
        envs=""
        [ $name = smc-race ] && envs="GEX_TEST_LIFT_DELAY_US=300"
        # null-access needs the null guard, a debug opt-in (gex_abi::null_guard).
        [ $name = null-access ] && envs="GEX_NULL_GUARD=1"
        [ $name = page-cross-entry ] && [ $tier = page ] && envs="GEX_TEST_UNPUBLISHED_RETIRE=1"
        # exec-signals and exec-futex exec themselves: in the page, from the guest's filesystem.
        pageopts=""
        { [ $name = exec-signals ] || [ $name = exec-futex ] || [ $name = spawn-threads ] || [ $name = proc-threads ] || [ $name = fs-threads ] || [ $name = exec-dethread ] || [ $name = fork-thread-ctx ]; } && pageopts="--file /bin/$name=$bin --argv0 /bin/$name"
        [ $name = served-entries ] && [ $tier = page ] && envs="GEX_TEST_SERVE_UNMERGED=1" &&
            pageopts="--file /bin/$name=$bin --file /bin/$name-b=$bin-b --argv0 /bin/$name"
        if [ $tier = page ]; then
            env $envs GEX_ARCH=x86 GEX_SHARED=1 GEX_MEM64=1 timeout 120 node shims/browser/js/run-node.mjs $pageopts "$SEED64" "$bin" >"$WORK/$name.page" 2>"$WORK/$name.page.err"; gs=$?
            # 125: run-node's flaky "nothing could wake it" after a threaded
            # program has finished (OPTIMIZATION-BACKLOG.md); once more.
            if [ $gs = 125 ]; then
                echo "     $name (page): exit 125, once more"
                env $envs GEX_ARCH=x86 GEX_SHARED=1 GEX_MEM64=1 timeout 120 node shims/browser/js/run-node.mjs $pageopts "$SEED64" "$bin" >"$WORK/$name.page" 2>/dev/null; gs=$?
            fi
            gex=$(grep -v '^\[host\]' "$WORK/$name.page")
            if [ $name = fork-dirs ] && grep -q 'gex: heal' "$WORK/$name.page.err"; then
                grep 'gex: heal' "$WORK/$name.page.err" | head -3
                gex="$gex (heal lines)"
            fi
        else
            gex=$(env $envs "$GEXC" run $flags "$bin" 2>/dev/null); gs=$?
        fi
        if [ "$native" = "$gex" ] && [ $ns = $gs ]; then
            echo "ok   $name ($target, $tier, exit $ns)"
        else
            echo "FAIL $name ($target, $tier): native exit $ns, gex exit $gs"
            printf '%s\n' "$native" > "$WORK/$name-$target.native"
            printf '%s\n' "$gex" > "$WORK/$name-$target.gex"
            diff "$WORK/$name-$target.native" "$WORK/$name-$target.gex" | head -20
            fail=1
        fi
    done
    done
done
exit $fail
