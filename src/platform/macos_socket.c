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
