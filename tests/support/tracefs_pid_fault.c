/* Test-only tracefs PID-filter contract model. Never loaded by production.
 * Real CPython/os calls on a temporary file; SEEK_END is EINVAL, writes add
 * tokens unless O_TRUNC was requested. This is not a physical-kernel test. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/types.h>
static int selected(int fd) {
    const char *wanted=getenv("GDP_PID_TEST_CONTROL");char link[64],path[4096];
    if(!wanted)return 0;
    snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
    ssize_t n=readlink(link,path,sizeof(path)-1);if(n<0)return 0;path[n]=0;
    return !strcmp(path,wanted);
}
static void receipt(const char *op,int fd,int result) {
    const char *path=getenv("GDP_PID_TEST_LOG");if(!path)return;
    int saved=errno;int out=open(path,O_WRONLY|O_CREAT|O_APPEND,0600);
    if(out>=0){char raw[256];int n=snprintf(raw,sizeof(raw),"%s flags=%d result=%d errno=%d\n",op,fcntl(fd,F_GETFL),result,saved);
        ssize_t (*real)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");(void)real(out,raw,(size_t)n);close(out);}
    errno=saved;
}
off_t lseek(int fd,off_t offset,int whence) {
    if(selected(fd)&&whence==SEEK_END){errno=EINVAL;receipt("lseek_end",fd,-1);return -1;}
    off_t (*real)(int,off_t,int)=dlsym(RTLD_NEXT,"lseek");return real(fd,offset,whence);
}
off64_t lseek64(int fd,off64_t offset,int whence) {
    if(selected(fd)&&whence==SEEK_END){errno=EINVAL;receipt("lseek_end",fd,-1);return -1;}
    off64_t (*real)(int,off64_t,int)=dlsym(RTLD_NEXT,"lseek64");return real(fd,offset,whence);
}
ssize_t write(int fd,const void *data,size_t count) {
    ssize_t (*real)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");
    if(!selected(fd))return real(fd,data,count);
    const char *fault=getenv("GDP_PID_TEST_FAULT");
    if(fault&&!strcmp(fault,"eio")){errno=EIO;receipt("write",fd,-1);return -1;}
    if(fault&&!strcmp(fault,"short")){receipt("write",fd,1);return 1;}
    off_t (*seek)(int,off_t,int)=dlsym(RTLD_NEXT,"lseek");
    if(seek(fd,0,SEEK_END)<0)return -1;
    ssize_t n=real(fd,data,count);receipt("write",fd,(int)n);return n;
}
