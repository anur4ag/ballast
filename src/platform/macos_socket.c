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
