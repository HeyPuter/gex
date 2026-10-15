/* The filesystem and descriptor surface (PLAN §8).
 *
 * Everything here has an observable answer that must match Linux exactly:
 * directory listings, stat, symlink traversal, rename, pipes, a shared
 * descriptor offset after dup, the synthetic /proc and /dev, and relative
 * path resolution after chdir. Output is sorted and contains no addresses,
 * so it can be compared byte for byte against QEMU.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <dirent.h>
#include <errno.h>
#include <sys/stat.h>
#include <poll.h>
int main(void){
    /* Directory listing */
    if (mkdir("/tmp/gexvfs", 0755) && errno != EEXIST) { perror("mkdir"); return 1; }
    for (int i=0;i<3;i++){ char p[64]; snprintf(p,sizeof p,"/tmp/gexvfs/f%d",i);
        int fd=open(p,O_CREAT|O_WRONLY|O_TRUNC,0644); if(fd<0){perror(p);return 1;}
        dprintf(fd,"file %d\n",i); close(fd); }
    DIR *d = opendir("/tmp/gexvfs"); if(!d){perror("opendir");return 1;}
    int n=0; struct dirent *e; char names[16][64]; 
    while((e=readdir(d))) { if(e->d_name[0]!='.') snprintf(names[n++],64,"%s",e->d_name); }
    closedir(d);
    for(int i=0;i<n;i++) for(int j=i+1;j<n;j++) if(strcmp(names[i],names[j])>0){char t[64];strcpy(t,names[i]);strcpy(names[i],names[j]);strcpy(names[j],t);}
    printf("dirents=%d:", n); for(int i=0;i<n;i++) printf(" %s", names[i]); printf("\n");

    /* stat */
    struct stat st; if(stat("/tmp/gexvfs/f0",&st)){perror("stat");return 1;}
    printf("stat size=%lld isreg=%d\n",(long long)st.st_size, S_ISREG(st.st_mode));
    if(stat("/tmp/gexvfs",&st)){perror("stat dir");return 1;}
    printf("statdir isdir=%d\n", S_ISDIR(st.st_mode));

    /* symlink + readlink */
    unlink("/tmp/gexvfs/link");
    if(symlink("f0","/tmp/gexvfs/link")){perror("symlink");return 1;}
    char lb[64]; ssize_t ln=readlink("/tmp/gexvfs/link",lb,sizeof lb-1); lb[ln>0?ln:0]=0;
    printf("readlink=%s\n", lb);
    FILE *lf=fopen("/tmp/gexvfs/link","r"); char line[64]; fgets(line,sizeof line,lf); fclose(lf);
    printf("through-link=%s", line);

    /* Open flags in this architecture's spelling. AArch64's O_LARGEFILE
     * (which glibc's open and fopen pass on every call) is x86's O_NOFOLLOW;
     * read as x86's, every open through a symlink was ELOOP. */
    int lfd=open("/tmp/gexvfs/link",O_RDONLY|O_LARGEFILE);
    printf("largefile-through-link=%d\n", lfd>=0); if(lfd>=0) close(lfd);
    errno=0; lfd=open("/tmp/gexvfs/link",O_RDONLY|O_NOFOLLOW);
    printf("nofollow-link=%d eloop=%d\n", lfd>=0, errno==ELOOP); if(lfd>=0) close(lfd);
    lfd=open("/tmp/gexvfs",O_RDONLY|O_DIRECTORY);
    printf("directory=%d\n", lfd>=0); if(lfd>=0) close(lfd);
    lfd=open("/tmp/gexvfs/f0",O_RDONLY); fcntl(lfd,F_SETFL,O_NONBLOCK); int fl=fcntl(lfd,F_GETFL);
    printf("getfl nonblock=%d directory=%d nofollow=%d\n", !!(fl&O_NONBLOCK), !!(fl&O_DIRECTORY), !!(fl&O_NOFOLLOW));
    close(lfd);

    /* rename + unlink */
    rename("/tmp/gexvfs/f2","/tmp/gexvfs/renamed");
    printf("renamed-exists=%d old-gone=%d\n", access("/tmp/gexvfs/renamed",F_OK)==0, access("/tmp/gexvfs/f2",F_OK)!=0);

    /* pipe */
    int pfd[2]; if(pipe(pfd)){perror("pipe");return 1;}
    write(pfd[1],"piped",5); close(pfd[1]);
    char pb[16]; int pn=read(pfd[0],pb,sizeof pb); pb[pn]=0; close(pfd[0]);
    printf("pipe=%s len=%d\n", pb, pn);

    /* dup and shared offset */
    int fd=open("/tmp/gexvfs/f0",O_RDONLY); int fd2=dup(fd);
    char c1,c2; read(fd,&c1,1); read(fd2,&c2,1);
    printf("dup-shared-offset=%d (%c%c)\n", c1=='f'&&c2=='i', c1, c2);
    close(fd); close(fd2);

    /* /proc */
    FILE *mp=fopen("/proc/self/maps","r"); int lines=0; char mb[256];
    while(fgets(mb,sizeof mb,mp)) lines++; fclose(mp);
    printf("proc-maps-lines>0=%d\n", lines>0);
    char eb[256]; ssize_t en=readlink("/proc/self/exe",eb,sizeof eb-1); eb[en>0?en:0]=0;
    printf("proc-exe=%s\n", eb);
    FILE *cl=fopen("/proc/self/cmdline","r"); size_t cn=fread(mb,1,sizeof mb,cl); fclose(cl);
    printf("proc-cmdline=%s (%zu)\n", mb, cn);

    /* /dev */
    int zf=open("/dev/zero",O_RDONLY); char zb[4]={1,2,3,4}; read(zf,zb,4); close(zf);
    printf("dev-zero=%d%d%d%d\n",zb[0],zb[1],zb[2],zb[3]);
    int nf=open("/dev/null",O_WRONLY); printf("dev-null-write=%zd\n", write(nf,"x",1)); close(nf);

    /* getcwd / chdir */
    char cw[256]; if(chdir("/tmp/gexvfs")){perror("chdir");return 1;}
    getcwd(cw,sizeof cw); printf("cwd=%s\n", cw);
    int rf=open("f0",O_RDONLY); printf("relative-open=%d\n", rf>=0); close(rf);

    /* poll on a regular file: always ready */
    fd=open("/tmp/gexvfs/f0",O_RDONLY);
    struct pollfd pf={.fd=fd,.events=POLLIN};
    printf("poll=%d revents-in=%d\n", poll(&pf,1,0), (pf.revents&POLLIN)!=0);
    close(fd);

    /* cleanup */
    for(int i=0;i<3;i++){ char p[64]; snprintf(p,sizeof p,"/tmp/gexvfs/f%d",i); unlink(p); }
    unlink("/tmp/gexvfs/renamed"); unlink("/tmp/gexvfs/link");
    chdir("/tmp"); rmdir("/tmp/gexvfs");
    printf("cleaned=%d\n", access("/tmp/gexvfs",F_OK)!=0);
    return 0;
}
