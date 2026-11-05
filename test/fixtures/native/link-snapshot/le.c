#include <stdio.h>
#include <string.h>
#include <unistd.h>
/* Execs itself once in the same process; the second image says it ran. */
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "again")) { printf("again ran\n"); return 0; }
    execl("/bin/le", "/bin/le", "again", (char *)0);
    return 1;
}
