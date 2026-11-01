/* dirent.h for Windows programs (gexcc-win), MinGW's layout: opendir and the
 * rest over FindFirstFileA (rt/dirent.c). */
#pragma once
#include <io.h>
#ifdef __cplusplus
extern "C" {
#endif
struct dirent {
    long d_ino;
    unsigned short d_reclen;
    unsigned short d_namlen;
    char d_name[260];
};
typedef struct gexw_dir DIR;
DIR *opendir(const char *path);
struct dirent *readdir(DIR *dir);
int closedir(DIR *dir);
void rewinddir(DIR *dir);
#ifdef __cplusplus
}
#endif
