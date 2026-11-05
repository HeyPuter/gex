/* A console program that prints a line and exits 3: a run with nothing
 * translated (the page's aot=1) has to carry it to its exit. */
#include <stdio.h>
int main(void) {
    printf("aotexit: ok 3\n");
    fflush(stdout);
    return 3;
}
