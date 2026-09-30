/* A console program on the UCRT: printf, argv, and an exit code of 42. */
#include <stdio.h>
int main(int argc, char **argv) {
    printf("hello from %s with %d args\n", argc > 0 ? "a Windows program" : "?", argc);
    for (int i = 1; i < argc; i++) printf("arg %d: %s\n", i, argv[i]);
    return 42;
}
