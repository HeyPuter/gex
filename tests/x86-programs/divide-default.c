/* A division by zero with no SIGFPE handler kills the process with SIGFPE
 * (exit status 136 from a shell). */
int main(int argc, char **argv) {
    (void)argv;
    volatile int zero = argc - 1;
    return 7 / zero;
}
