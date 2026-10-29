/* Linux-only, per-child IO fault injection. Never changes the parent process. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
static _Atomic unsigned count;
static int fail(const char *op, const char *path) {
    const char *armed=getenv("GDP_IO_ARMED");
    if(armed && access(armed,F_OK)!=0)return 0;
    const char *wanted=getenv("GDP_IO_FAIL_OP");
    const char *suffix=getenv("GDP_IO_FAIL_SUFFIX");
    const char *contains=getenv("GDP_IO_FAIL_CONTAINS");
    if(!wanted || strcmp(wanted,op) || !path) return 0;
    if(suffix) {size_t n=strlen(path),m=strlen(suffix);if(n<m || strcmp(path+n-m,suffix))return 0;}
    if(contains && !strstr(path,contains))return 0;
    const char *nth=getenv("GDP_IO_FAIL_NTH");
    if(atomic_fetch_add(&count,1)+1 != (nth?(unsigned)atoi(nth):1))return 0;
    const char *marker=getenv("GDP_IO_FAULT_MARKER");
    if(marker) {
        int fd=open(marker,O_WRONLY|O_CREAT|O_EXCL,0600);
        if(fd>=0) {ssize_t (*real_write)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");(void)real_write(fd,op,strlen(op));close(fd);}
    }
    const char *grow=getenv("GDP_IO_GROW_BYTES");
    if(grow) {
        int fd=open(path,O_WRONLY|O_NOFOLLOW);
        if(fd>=0){(void)ftruncate(fd,(off_t)strtoull(grow,NULL,10));close(fd);}
        return 0;
    }
    const char *delay=getenv("GDP_IO_DELAY_MS");
    if(delay){usleep((useconds_t)strtoul(delay,NULL,10)*1000);return 0;}
    const char *error=getenv("GDP_IO_ERRNO");
    errno=error&&!strcmp(error,"EINTR")?EINTR:EIO;return 1;
}
static int fail_fd(const char *op,int fd) {
    char link[64],path[4096];
    snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
    ssize_t n=readlink(link,path,sizeof(path)-1);
    if(n<0)return 0;
    path[n]=0;return fail(op,path);
}
ssize_t write(int fd,const void *buf,size_t n) {
    if(fail_fd("write",fd))return -1;
    ssize_t (*real)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");return real(fd,buf,n);
}
int fsync(int fd) {
    if(fail_fd("fsync",fd))return -1;
    int (*real)(int)=dlsym(RTLD_NEXT,"fsync");return real(fd);
}
int linkat(int a,const char *b,int c,const char *d,int e) {
    if(fail("linkat",d))return -1;
    int (*real)(int,const char*,int,const char*,int)=dlsym(RTLD_NEXT,"linkat");return real(a,b,c,d,e);
}
int sigaction(int sig,const struct sigaction *a,struct sigaction *b) {
    if(fail("sigaction",sig==SIGTERM?"SIGTERM":"other"))return -1;
    int (*real)(int,const struct sigaction*,struct sigaction*)=dlsym(RTLD_NEXT,"sigaction");return real(sig,a,b);
}
int waitid(idtype_t type,id_t id,siginfo_t *info,int options) {
    if(fail("waitid","worker"))return -1;
    int (*real)(idtype_t,id_t,siginfo_t*,int)=dlsym(RTLD_NEXT,"waitid");return real(type,id,info,options);
}
ssize_t read(int fd,void *buf,size_t n) {
    if(fail_fd("read",fd)) {
        const char *fake=getenv("GDP_IO_FAKE_READ");
        if(fake){size_t size=strlen(fake);if(size>n)size=n;memcpy(buf,fake,size);(void)lseek(fd,0,SEEK_END);return (ssize_t)size;}
        return -1;
    }
    ssize_t (*real)(int,void*,size_t)=dlsym(RTLD_NEXT,"read");return real(fd,buf,n);
}

int open64(const char *path,int flags,...) {
    if(fail("open",path))return -1;
    mode_t mode=0;
    if((flags&O_CREAT) || ((flags&O_TMPFILE)==O_TMPFILE)){va_list a;va_start(a,flags);mode=va_arg(a,mode_t);va_end(a);}
    int (*real)(const char*,int,...)=dlsym(RTLD_NEXT,"open64");return real(path,flags,mode);
}
int openat64(int dir,const char *path,int flags,...) {
    if(fail("open",path))return -1;
    mode_t mode=0;
    if((flags&O_CREAT) || ((flags&O_TMPFILE)==O_TMPFILE)){va_list a;va_start(a,flags);mode=va_arg(a,mode_t);va_end(a);}
    int (*real)(int,const char*,int,...)=dlsym(RTLD_NEXT,"openat64");return real(dir,path,flags,mode);
}

int pthread_create(pthread_t *thread,const pthread_attr_t *attr,void *(*start)(void*),void *arg) {
    if(fail("pthread_create","raw-writer"))return EAGAIN;
    int (*real)(pthread_t*,const pthread_attr_t*,void*(*)(void*),void*)=dlsym(RTLD_NEXT,"pthread_create");
    return real(thread,attr,start,arg);
}
int mkdir(const char *path,mode_t mode) {
    if(fail("mkdir",path))return -1;
    int (*real)(const char*,mode_t)=dlsym(RTLD_NEXT,"mkdir");return real(path,mode);
}
