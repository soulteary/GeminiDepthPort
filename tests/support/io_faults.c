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
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/uio.h>
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
    if(atomic_fetch_add(&count,1)+1 != (nth?(unsigned)atoi(nth):1) && !getenv("GDP_IO_REPEAT"))return 0;
    const char *marker=getenv("GDP_IO_FAULT_MARKER");
    if(marker) {
        int fd=open(marker,O_WRONLY|O_CREAT|O_EXCL,0600);
        if(fd>=0) {ssize_t (*real_write)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");(void)real_write(fd,op,strlen(op));close(fd);}
    }
    const char *release=getenv("GDP_IO_RELEASE");
    if(release) {unsigned waited=0;while(access(release,F_OK)!=0 && waited++<10000)usleep(1000);}
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
    path[n]=0;
    const char *type=getenv("GDP_IO_FAIL_FD_TYPE");
    if(type) {
        struct stat st;if(fstat(fd,&st)!=0)return 0;
        if((!strcmp(type,"directory")&&!S_ISDIR(st.st_mode))||(!strcmp(type,"file")&&!S_ISREG(st.st_mode)))return 0;
    }
    const char *content=getenv("GDP_IO_REQUIRE_CONTENT");
    if(content) {
        int input=open(path,O_RDONLY);if(input<0)return 0;
        char raw[65536];ssize_t (*real_read)(int,void*,size_t)=dlsym(RTLD_NEXT,"read");
        ssize_t size=real_read(input,raw,sizeof(raw)-1);close(input);
        if(size<0)return 0;
        raw[size]=0;
        if(!strstr(raw,content))return 0;
    }
    return fail(op,path);
}
static _Atomic unsigned long long payload_writes,payload_writevs,index_writes,index_writevs;
static int payload_fd(int fd) {
    char link[64],path[4096];snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
    ssize_t n=readlink(link,path,sizeof(path)-1);if(n<0)return 0;path[n]=0;
    return strstr(path,"/payload-")!=NULL;
}
static void trace_write(int fd,int vectored) {
    if(!getenv("GDP_IO_WRITE_COUNTS"))return;
    char link[64],path[4096];snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
    ssize_t n=readlink(link,path,sizeof(path)-1);if(n<0)return;path[n]=0;
    if(strstr(path,"/payload-")) {if(vectored)atomic_fetch_add(&payload_writevs,1);else atomic_fetch_add(&payload_writes,1);}
    else if(n>=14 && !strcmp(path+n-14,"/records.jsonl")) {if(vectored)atomic_fetch_add(&index_writevs,1);else atomic_fetch_add(&index_writes,1);}
}
__attribute__((destructor)) static void save_write_counts(void) {
    const char *path=getenv("GDP_IO_WRITE_COUNTS");if(!path)return;
    char raw[256];int n=snprintf(raw,sizeof(raw),"{\"payload_write\":%llu,\"payload_writev\":%llu,\"index_write\":%llu,\"index_writev\":%llu}",
        atomic_load(&payload_writes),atomic_load(&payload_writevs),atomic_load(&index_writes),atomic_load(&index_writevs));
    int fd=open(path,O_WRONLY|O_CREAT|O_TRUNC,0600);
    if(fd>=0){ssize_t (*real)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");(void)real(fd,raw,(size_t)n);close(fd);}
}
ssize_t writev(int fd,const struct iovec *iov,int count_iov) {
    trace_write(fd,1);
    if(fail_fd("writev",fd))return -1;
    ssize_t (*real)(int,const struct iovec*,int)=dlsym(RTLD_NEXT,"writev");
    const char *cap=getenv("GDP_IO_WRITEV_MAX_BYTES"),*armed=getenv("GDP_IO_ARMED");
    if(cap && (!armed || access(armed,F_OK)==0) && count_iov>=0 && count_iov<=16 && payload_fd(fd)) {
        size_t remaining=(size_t)strtoull(cap,NULL,10);struct iovec limited[16];int count_limited=0;
        for(int i=0;i<count_iov && remaining;i++) {
            size_t n=iov[i].iov_len;if(n>remaining)n=remaining;
            if(n){limited[count_limited]=iov[i];limited[count_limited++].iov_len=n;remaining-=n;}
        }
        return real(fd,limited,count_limited);
    }
    return real(fd,iov,count_iov);
}
ssize_t write(int fd,const void *buf,size_t n) {
    trace_write(fd,0);
    if(fail_fd("write",fd))return -1;
    ssize_t (*real)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");
    const char *cap=getenv("GDP_IO_WRITE_MAX_BYTES"),*armed=getenv("GDP_IO_ARMED");
    if(cap && (!armed || access(armed,F_OK)==0)) {
        char link[64],path[4096];snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
        ssize_t len=readlink(link,path,sizeof(path)-1);
        if(len>=14){path[len]=0;if(!strcmp(path+len-14,"/records.jsonl")){size_t max=(size_t)strtoull(cap,NULL,10);if(n>max)n=max;}}
    }
    return real(fd,buf,n);
}
static _Atomic unsigned long long sync_calls[5][3];
static void count_sync(int fd,int result,int injected) {
    if(!getenv("GDP_IO_SYNC_COUNTS"))return;
    int role=4;struct stat st;char link[64],path[4096];
    snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
    ssize_t n=readlink(link,path,sizeof(path)-1);
    if(n>=0&&fstat(fd,&st)==0){path[n]=0;int quality=strstr(path,"/quality/")!=NULL;
        if(S_ISREG(st.st_mode))role=quality?0:2;else if(S_ISDIR(st.st_mode))role=quality?1:3;}
    atomic_fetch_add(&sync_calls[role][injected?2:result==0?0:1],1);
}
__attribute__((destructor)) static void save_sync_counts(void) {
    const char *prefix=getenv("GDP_IO_SYNC_COUNTS");if(!prefix)return;
    char path[4096],raw[1024];int size=snprintf(path,sizeof(path),"%s.%ld.json",prefix,(long)getpid());
    if(size<0||(size_t)size>=sizeof(path))return;
    const char *roles[]={"quality_file","quality_directory","other_file","other_directory","unknown"};
    int used=snprintf(raw,sizeof(raw),"{\"pid\":%ld,\"roles\":{",(long)getpid());
    for(int i=0;i<5;i++)used+=snprintf(raw+used,sizeof(raw)-(size_t)used,"%s\"%s\":{\"real_success\":%llu,\"real_error\":%llu,\"injected_error\":%llu}",i?",":"",roles[i],atomic_load(&sync_calls[i][0]),atomic_load(&sync_calls[i][1]),atomic_load(&sync_calls[i][2]));
    used+=snprintf(raw+used,sizeof(raw)-(size_t)used,"}}\n");
    int fd=open(path,O_WRONLY|O_CREAT|O_TRUNC,0600);
    if(fd>=0){ssize_t (*real)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");(void)real(fd,raw,(size_t)used);close(fd);}
}
int fsync(int fd) {
    int injected=fail_fd("fsync",fd),result=-1;
    if(!injected){int (*real)(int)=dlsym(RTLD_NEXT,"fsync");result=real(fd);}
    int saved=errno;count_sync(fd,result,injected);errno=saved;return result;
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
    /* Independent final-reader barrier for overlapping status-fsync faults. */
    const char *hold=getenv("GDP_IO_HOLD_READ_SUFFIX");
    if(hold) {
        char link[64],path[4096];snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
        ssize_t len=readlink(link,path,sizeof(path)-1);
        if(len>=0) {
            path[len]=0;size_t suffix=strlen(hold);
            if((size_t)len>=suffix&&!strcmp(path+len-suffix,hold)) {
                const char *marker=getenv("GDP_IO_HOLD_MARKER"),*release=getenv("GDP_IO_HOLD_RELEASE");
                if(marker&&release) {
                    int out=open(marker,O_WRONLY|O_CREAT|O_EXCL,0600);
                    if(out>=0){ssize_t (*real_write)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");(void)real_write(out,"read",4);close(out);}
                    unsigned waited=0;while(access(release,F_OK)!=0&&waited++<10000)usleep(1000);
                }
            }
        }
    }
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
int rename(const char *oldpath,const char *newpath) {
    if(fail("rename",newpath))return -1;
    int (*real)(const char*,const char*)=dlsym(RTLD_NEXT,"rename");
    return real(oldpath,newpath);
}
