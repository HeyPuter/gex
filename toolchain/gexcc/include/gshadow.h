/* glibc's <gshadow.h>: /etc/gshadow, the groups' shadow file. musl has
 * none; the functions are in libgexcc (rt/gshadow.c). systemd's user
 * records read it, and sysusers writes it. */
#ifndef _GEXCC_GSHADOW_H
#define _GEXCC_GSHADOW_H

#include <stddef.h>
#include <stdio.h>

#ifdef __cplusplus
extern "C" {
#endif

#define GSHADOW "/etc/gshadow"

struct sgrp {
    char *sg_namp;   /* the group's name */
    char *sg_passwd; /* its encrypted password */
    char **sg_adm;   /* its administrators */
    char **sg_mem;   /* its members */
};

void setsgent(void);
void endsgent(void);
struct sgrp *getsgent(void);
struct sgrp *getsgnam(const char *name);
struct sgrp *sgetsgent(const char *line);
struct sgrp *fgetsgent(FILE *stream);
int putsgent(const struct sgrp *g, FILE *stream);
int getsgent_r(struct sgrp *res, char *buf, size_t len, struct sgrp **out);
int getsgnam_r(const char *name, struct sgrp *res, char *buf, size_t len, struct sgrp **out);
int sgetsgent_r(const char *line, struct sgrp *res, char *buf, size_t len, struct sgrp **out);
int fgetsgent_r(FILE *stream, struct sgrp *res, char *buf, size_t len, struct sgrp **out);

#ifdef __cplusplus
}
#endif

#endif
