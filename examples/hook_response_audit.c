/* First complete IPC frame seen by the real hook process, without proxying its peer. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>
#ifndef __APPLE__
#include <dlfcn.h>
#endif

static int output = -1;
static unsigned long long began;
static _Thread_local char frame[16384];
static _Thread_local size_t used;
static _Thread_local int recorded;

static unsigned long long now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec * 1000000000ULL + t.tv_nsec;
}

__attribute__((constructor)) static void initialize(void) {
    const char *path = getenv("BALLAST_HOOK_AUDIT");
    began = now_ns();
    if (path) output = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
    if (output >= 0) {
        char text[160];
        int n = snprintf(text, sizeof(text), "{\"event\":\"loaded\",\"pid\":%d,\"ns\":%llu}\n", getpid(), began);
        (void)write(output, text, (size_t)n);
    }
}

static void observe(int fd, const void *buffer, ssize_t size) {
    int saved = errno, type;
    socklen_t length = sizeof(type);
    if (output < 0 || recorded || size <= 0 || getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &length)) goto done;
    size_t count = (size_t)size;
    if (count > sizeof(frame)-used-1) count = sizeof(frame)-used-1;
    memcpy(frame+used, buffer, count);
    used += count;
    frame[used] = 0;
    if (!strchr(frame, '\n')) goto done;
    recorded = 1;
    const char *decision = strstr(frame, "\"decision\":\"hold\"") ? "hold" :
                           strstr(frame, "\"decision\":\"deny\"") ? "deny" :
                           strstr(frame, "\"decision\":\"admit\"") ? "admit" : "unknown";
    char text[240];
    unsigned long long at = now_ns();
    int n = snprintf(text, sizeof(text), "{\"event\":\"first_response\",\"pid\":%d,\"ns\":%llu,\"elapsed_ns\":%llu,\"decision\":\"%s\"}\n", getpid(), at, at-began, decision);
    (void)write(output, text, (size_t)n);
done:
    errno = saved;
}

#ifdef __APPLE__
static ssize_t audit_read(int fd, void *buffer, size_t count) {
    ssize_t n = read(fd, buffer, count);
    observe(fd, buffer, n);
    return n;
}
static ssize_t audit_recv(int fd, void *buffer, size_t count, int flags) {
    ssize_t n = recv(fd, buffer, count, flags);
    observe(fd, buffer, n);
    return n;
}
#define INTERPOSE(replacement, original) \
    __attribute__((used)) static struct { const void *replace; const void *original; } pair_##original \
    __attribute__((section("__DATA,__interpose"))) = { (const void *)(replacement), (const void *)(original) }
INTERPOSE(audit_read, read);
INTERPOSE(audit_recv, recv);
#else
ssize_t read(int fd, void *buffer, size_t count) {
    static ssize_t (*original)(int, void *, size_t);
    if (!original) original = dlsym(RTLD_NEXT, "read");
    ssize_t n = original(fd, buffer, count);
    observe(fd, buffer, n);
    return n;
}
ssize_t recv(int fd, void *buffer, size_t count, int flags) {
    static ssize_t (*original)(int, void *, size_t, int);
    if (!original) original = dlsym(RTLD_NEXT, "recv");
    ssize_t n = original(fd, buffer, count, flags);
    observe(fd, buffer, n);
    return n;
}
#endif
