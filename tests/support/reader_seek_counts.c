#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>
/* Read-only payload seek counting, armed after COMPLETE; never a CPU benchmark. */
static _Atomic unsigned long long seeks;
static void count_seek(int fd) {
    const char *armed=getenv("GDP_READER_SEEK_ARMED");
    if(!armed || access(armed,F_OK)!=0)return;
    char link[64],path[4096];
    snprintf(link,sizeof(link),"/proc/self/fd/%d",fd);
    ssize_t n=readlink(link,path,sizeof(path)-1);
    if(n<0)return;
    path[n]=0;
    if(strstr(path,"/payload-"))atomic_fetch_add(&seeks,1);
}
off_t lseek(int fd,off_t offset,int whence) {
    count_seek(fd);
    off_t (*real)(int,off_t,int)=dlsym(RTLD_NEXT,"lseek");
    return real(fd,offset,whence);
}
off64_t lseek64(int fd,off64_t offset,int whence) {
    count_seek(fd);
    off64_t (*real)(int,off64_t,int)=dlsym(RTLD_NEXT,"lseek64");
    return real(fd,offset,whence);
}
__attribute__((destructor)) static void save_counts(void) {
    const char *path=getenv("GDP_READER_SEEK_COUNTS");
    if(!path)return;
    char raw[128];
    int n=snprintf(raw,sizeof(raw),"{\"payload_seek_calls\":%llu}",atomic_load(&seeks));
    int fd=open(path,O_WRONLY|O_CREAT|O_EXCL,0600);
    if(fd>=0) {
        ssize_t (*real)(int,const void*,size_t)=dlsym(RTLD_NEXT,"write");
        (void)real(fd,raw,(size_t)n);close(fd);
    }
}
