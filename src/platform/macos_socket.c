#include <libproc.h>
#include <arpa/inet.h>
#include <errno.h>

/* Use the SDK's socket union layout rather than duplicating its ABI in Rust. */
int ballast_listening_port(int pid, int fd) {
    struct socket_fdinfo info = {0};
    int size = proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, &info, sizeof(info));
    if (size != sizeof(info)) return -1;
    if (info.psi.soi_kind != SOCKINFO_TCP ||
        (info.psi.soi_family != AF_INET && info.psi.soi_family != AF_INET6) ||
        info.psi.soi_proto.pri_tcp.tcpsi_state != TSI_S_LISTEN) return 0;
    return ntohs((uint16_t)info.psi.soi_proto.pri_tcp.tcpsi_ini.insi_lport);
}

#include <string.h>
int ballast_process_cwd(int pid, char *path, size_t capacity) {
    struct proc_vnodepathinfo info = {0};
    int size = proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, &info, sizeof(info));
    if (size != sizeof(info)) return -1;
    size_t length = strnlen(info.pvi_cdir.vip_path, sizeof(info.pvi_cdir.vip_path));
    if (length == 0 || length >= capacity) return -1;
    memcpy(path, info.pvi_cdir.vip_path, length);
    path[length] = 0;
    return 0;
}

#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOKitLib.h>

/* Aggregate driver counters; the registry fingerprint detects device changes. */
int ballast_disk_counters(uint64_t *time_ns, uint64_t *bytes, uint64_t *fingerprint) {
    io_iterator_t iterator;
    if (IOServiceGetMatchingServices(MACH_PORT_NULL,
            IOServiceMatching("IOBlockStorageDriver"), &iterator) != KERN_SUCCESS) return -1;
    *time_ns = *bytes = *fingerprint = 0;
    int found = 0;
    io_object_t device;
    while ((device = IOIteratorNext(iterator))) {
        CFTypeRef stats = IORegistryEntryCreateCFProperty(device, CFSTR("Statistics"),
                                                         kCFAllocatorDefault, 0);
        uint64_t id = 0;
        if (stats && CFGetTypeID(stats) == CFDictionaryGetTypeID() &&
                IORegistryEntryGetRegistryEntryID(device, &id) == KERN_SUCCESS) {
            CFStringRef keys[] = { CFSTR("Total Time (Read)"), CFSTR("Total Time (Write)"),
                                   CFSTR("Bytes (Read)"), CFSTR("Bytes (Write)") };
            int64_t values[4] = {0};
            int valid = 1;
            for (int i = 0; i < 4; i++) {
                CFTypeRef value = CFDictionaryGetValue(stats, keys[i]);
                if (!value || CFGetTypeID(value) != CFNumberGetTypeID() ||
                    !CFNumberGetValue(value, kCFNumberSInt64Type, &values[i]) || values[i] < 0)
                    valid = 0;
            }
            if (valid) {
                *time_ns += (uint64_t)values[0] + (uint64_t)values[1];
                *bytes += (uint64_t)values[2] + (uint64_t)values[3];
                *fingerprint ^= id;
                found++;
            }
        }
        if (stats) CFRelease(stats);
        IOObjectRelease(device);
    }
    IOObjectRelease(iterator);
    return found ? 0 : -1;
}
