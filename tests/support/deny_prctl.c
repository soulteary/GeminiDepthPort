/* Fault only this child and its descendants; no host policy is changed. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>
int main(int argc,char **argv) {
    if(argc<3)return 2;
    unsigned target;
    if(!strcmp(argv[1],"pdeath"))target=PR_SET_PDEATHSIG;
    else if(!strcmp(argv[1],"subreaper"))target=PR_SET_CHILD_SUBREAPER;
    else return 2;
    struct sock_filter instructions[]={
        BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,nr)),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,SYS_prctl,0,3),
        BPF_STMT(BPF_LD|BPF_W|BPF_ABS,offsetof(struct seccomp_data,args[0])),
        BPF_JUMP(BPF_JMP|BPF_JEQ|BPF_K,target,0,1),
        BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ERRNO|EIO),
        BPF_STMT(BPF_RET|BPF_K,SECCOMP_RET_ALLOW)
    };
    struct sock_fprog program={sizeof(instructions)/sizeof(instructions[0]),instructions};
    if(prctl(PR_SET_NO_NEW_PRIVS,1,0,0,0) || prctl(PR_SET_SECCOMP,SECCOMP_MODE_FILTER,&program))return 77;
    const char *marker=getenv("GDP_SECCOMP_MARKER");
    if(marker){int fd=open(marker,O_CREAT|O_EXCL|O_WRONLY,0600);if(fd<0)return 3;close(fd);}
    execv(argv[2],&argv[2]);return 127;
}
