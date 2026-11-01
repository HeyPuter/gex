/* opendir/readdir/closedir/rewinddir over FindFirstFileA (include-pre/dirent.h) */
#include <windows.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <dirent.h>

struct gexw_dir {
    HANDLE find;
    WIN32_FIND_DATAA data;
    int first;
    struct dirent ent;
    char pattern[MAX_PATH + 3];
};

static int start(DIR *d)
{
    d->find = FindFirstFileA(d->pattern, &d->data);
    d->first = 1;
    return d->find != INVALID_HANDLE_VALUE;
}

DIR *opendir(const char *path)
{
    size_t n = path ? strlen(path) : 0;
    DIR *d;
    DWORD attr;
    if (!n || n > MAX_PATH) { errno = n ? ENAMETOOLONG : ENOENT; return NULL; }
    attr = GetFileAttributesA(path);
    if (attr == INVALID_FILE_ATTRIBUTES) { errno = ENOENT; return NULL; }
    if (!(attr & FILE_ATTRIBUTE_DIRECTORY)) { errno = ENOTDIR; return NULL; }
    if (!(d = calloc(1, sizeof(*d)))) { errno = ENOMEM; return NULL; }
    memcpy(d->pattern, path, n);
    if (path[n - 1] != '/' && path[n - 1] != '\\') d->pattern[n++] = '\\';
    d->pattern[n++] = '*';
    d->pattern[n] = 0;
    if (!start(d) && GetLastError() != ERROR_FILE_NOT_FOUND) { free(d); errno = EACCES; return NULL; }
    return d;
}

struct dirent *readdir(DIR *d)
{
    if (!d || d->find == INVALID_HANDLE_VALUE) return NULL;
    if (!d->first && !FindNextFileA(d->find, &d->data)) return NULL;
    d->first = 0;
    strncpy(d->ent.d_name, d->data.cFileName, sizeof(d->ent.d_name) - 1);
    d->ent.d_namlen = strlen(d->ent.d_name);
    d->ent.d_reclen = sizeof(d->ent);
    return &d->ent;
}

void rewinddir(DIR *d)
{
    if (!d) return;
    if (d->find != INVALID_HANDLE_VALUE) FindClose(d->find);
    start(d);
}

int closedir(DIR *d)
{
    if (!d) { errno = EBADF; return -1; }
    if (d->find != INVALID_HANDLE_VALUE) FindClose(d->find);
    free(d);
    return 0;
}
