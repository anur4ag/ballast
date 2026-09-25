#!/usr/bin/env python3
"""macOS spike for ticket 17, step 1.1: does PRIO_DARWIN_PROCESS / PRIO_DARWIN_BG
behave as the throttle lever needs on a process this script owns?

Answers, on owned processes only (this script's own forked children):
  1. setpriority(PRIO_DARWIN_PROCESS, pid, PRIO_DARWIN_BG) on ANOTHER process
     of the same user (not just self/who=0, which is all the setpriority(2)
     man page documents as supported).
  2. Clearing it (setpriority(..., pid, 0)) fully restores the original
     scheduling priority, not just a zero return code.
  3. Whether a child forked *after* the process is backgrounded inherits the
     background state.
  4. proc_pidinfo's pbi_flags (PROC_FLAG_DARWINBG / PROC_FLAG_EXT_DARWINBG),
     which distinguish a process's own vs. externally-imposed background
     state. getpriority(PRIO_DARWIN_PROCESS, other_pid) is NOT a reliable way
     to read another process's state on this OS: XNU's get_background_proc()
     (bsd/kern/kern_resource.c) resolves external vs. internal by the
     *target*, then calls proc_get_task_policy(current_task(), ...), which
     reads the CALLER's task rather than the target's -- so it always
     reports the caller's own state instead. proc_pidinfo does not have
     this bug and is used here as the reliable check.

None of this establishes disk I/O throttling is restored -- only CPU
scheduling priority and the BG flags are observed. Actual I/O QoS behavior
needs the approved-load harness runs in ticket 17 step 3, not this spike.

Run: python3 spikes/darwin_priority_probe.py
No product code, no root, no processes outside this script's own tree.
Exits 0 only if every assertion held; prints and exits non-zero otherwise.
"""
import ctypes
import os
import select
import signal
import subprocess
import sys
import time

PRIO_DARWIN_PROCESS = 4
PRIO_DARWIN_BG = 0x1000
PROC_PIDTBSDINFO = 3
PROC_FLAG_DARWINBG = 0x8000
PROC_FLAG_EXT_DARWINBG = 0x10000

libc = ctypes.CDLL(None, use_errno=True)
libc.proc_pidinfo.restype = ctypes.c_int
libc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]


class proc_bsdinfo(ctypes.Structure):
    _fields_ = [
        ("pbi_flags", ctypes.c_uint32),
        ("pbi_status", ctypes.c_uint32),
        ("pbi_xstatus", ctypes.c_uint32),
        ("pbi_pid", ctypes.c_uint32),
        ("pbi_ppid", ctypes.c_uint32),
        ("pbi_uid", ctypes.c_uint32),
        ("pbi_gid", ctypes.c_uint32),
        ("pbi_ruid", ctypes.c_uint32),
        ("pbi_rgid", ctypes.c_uint32),
        ("pbi_svuid", ctypes.c_uint32),
        ("pbi_svgid", ctypes.c_uint32),
        ("rfu_1", ctypes.c_uint32),
        ("pbi_comm", ctypes.c_char * 16),
        ("pbi_name", ctypes.c_char * 32),
        ("pbi_nfiles", ctypes.c_uint32),
        ("pbi_pgid", ctypes.c_uint32),
        ("pbi_pjobc", ctypes.c_uint32),
        ("e_tdev", ctypes.c_uint32),
        ("e_tpgid", ctypes.c_uint32),
        ("pbi_nice", ctypes.c_int32),
        ("pbi_start_tvsec", ctypes.c_uint64),
        ("pbi_start_tvusec", ctypes.c_uint64),
    ]


def setprio(pid: int, value: int) -> tuple[int, int]:
    ctypes.set_errno(0)
    result = libc.setpriority(PRIO_DARWIN_PROCESS, pid, value)
    return result, ctypes.get_errno()


def bg_flags(pid: int) -> dict:
    info = proc_bsdinfo()
    n = libc.proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, ctypes.byref(info), ctypes.sizeof(info))
    if n <= 0:
        raise OSError(ctypes.get_errno(), "proc_pidinfo(PROC_PIDTBSDINFO) failed")
    flags = info.pbi_flags
    return {
        "raw": hex(flags),
        "own_darwinbg": bool(flags & PROC_FLAG_DARWINBG),
        "external_darwinbg": bool(flags & PROC_FLAG_EXT_DARWINBG),
    }


def pri(pid: int) -> int | None:
    """Numeric scheduling priority only -- no `comm`/path fields, so nothing
    executable-path-shaped ever lands in committed spike output."""
    out = subprocess.run(
        ["ps", "-o", "pri=", "-p", str(pid)], capture_output=True, text=True
    ).stdout.strip()
    return int(out) if out else None


def read_line(fd: int, timeout: float = 5.0) -> str:
    ready, _, _ = select.select([fd], [], [], timeout)
    if not ready:
        raise TimeoutError(f"no data on fd {fd} within {timeout}s")
    data = os.read(fd, 256)
    if not data:
        raise EOFError("pipe closed unexpectedly")
    return data.decode().strip()


def basic_set_clear_check() -> None:
    print("1-2. setpriority(BG) on an owned child, then clear it:")
    child = subprocess.Popen(["sleep", "20"])
    pid = child.pid
    try:
        time.sleep(0.2)
        baseline_pri = pri(pid)
        baseline_flags = bg_flags(pid)
        print(f"   baseline: pri={baseline_pri} flags={baseline_flags}")

        set_result = setprio(pid, PRIO_DARWIN_BG)
        time.sleep(0.1)
        bg_pri = pri(pid)
        bg_flags_now = bg_flags(pid)
        print(f"   setpriority(BG) result={set_result} pri={bg_pri} flags={bg_flags_now}")
        assert set_result == (0, 0), f"setpriority(BG) on an owned child failed: {set_result}"
        assert bg_pri is not None and bg_pri < baseline_pri, (
            f"scheduling priority did not drop after BG: baseline={baseline_pri} after={bg_pri}"
        )
        assert bg_flags_now["external_darwinbg"], "PROC_FLAG_EXT_DARWINBG not set after external setpriority(BG)"
        assert not bg_flags_now["own_darwinbg"], "PROC_FLAG_DARWINBG (self-imposed) unexpectedly set by an external caller"

        clear_result = setprio(pid, 0)
        time.sleep(0.1)
        restored_pri = pri(pid)
        restored_flags = bg_flags(pid)
        print(f"   setpriority(clear) result={clear_result} pri={restored_pri} flags={restored_flags}")
        assert clear_result == (0, 0), f"setpriority(clear) failed: {clear_result}"
        assert restored_pri == baseline_pri, (
            f"clearing BG did not fully restore scheduling priority: baseline={baseline_pri} after={restored_pri}"
        )
        assert not restored_flags["external_darwinbg"], "PROC_FLAG_EXT_DARWINBG still set after clearing"
        print("   CPU scheduling priority and BG flags fully restored (I/O QoS restoration is NOT verified here).")
    finally:
        child.terminate()
        child.wait()


def fork_inheritance_check() -> None:
    """Fork an owned child with a command/ready pipe protocol (no signals,
    no races): tell it to fork a grandchild after being backgrounded, then
    tell it to reap that grandchild itself, since only the child -- not this
    harness -- is the grandchild's real parent. Every read has a timeout so
    a protocol bug hangs loudly instead of silently."""
    print("3. fork-after-background inheritance, via a command/ready pipe:")
    cmd_r, cmd_w = os.pipe()
    rdy_r, rdy_w = os.pipe()

    child_pid = os.fork()
    if child_pid == 0:
        os.close(cmd_w)
        os.close(rdy_r)
        grandchild = None  # tracked here so this process's own finally can
                            # reap it even if the parent never sends "reap"
                            # (e.g. it died mid-assertion after "fork").
        try:
            while True:
                try:
                    cmd = os.read(cmd_r, 64).decode().strip()
                except OSError:
                    break
                if not cmd or cmd == "exit":
                    break
                if cmd == "fork":
                    grandchild = os.fork()
                    if grandchild == 0:
                        os.close(cmd_r)
                        os.close(rdy_w)
                        os.execvp("sleep", ["sleep", "15"])
                    os.write(rdy_w, f"{grandchild}\n".encode())
                elif cmd.startswith("reap "):
                    gpid = int(cmd.split()[1])
                    try:
                        os.kill(gpid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    os.waitpid(gpid, 0)
                    grandchild = None
                    os.write(rdy_w, b"reaped\n")
        finally:
            if grandchild is not None:
                try:
                    os.kill(grandchild, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    os.waitpid(grandchild, 0)
                except ChildProcessError:
                    pass
            os._exit(0)

    os.close(cmd_r)
    os.close(rdy_w)
    try:
        time.sleep(0.2)
        set_result = setprio(child_pid, PRIO_DARWIN_BG)
        time.sleep(0.1)
        child_pri = pri(child_pid)
        print(f"   child pid {child_pid} setpriority(BG)={set_result} pri={child_pri}")
        assert set_result == (0, 0), f"setpriority(BG) failed: {set_result}"

        os.write(cmd_w, b"fork\n")
        grandchild = int(read_line(rdy_r))
        time.sleep(0.2)
        grandchild_pri = pri(grandchild)
        grandchild_flags = bg_flags(grandchild)
        print(f"   grandchild pid {grandchild} (forked after BG) pri={grandchild_pri} flags={grandchild_flags}")
        assert grandchild_pri is not None and grandchild_pri == child_pri, (
            f"grandchild did not inherit backgrounded priority: parent={child_pri} grandchild={grandchild_pri}"
        )
        assert grandchild_flags["external_darwinbg"], "grandchild did not inherit PROC_FLAG_EXT_DARWINBG"

        os.write(cmd_w, f"reap {grandchild}\n".encode())
        ack = read_line(rdy_r)
        assert ack == "reaped", f"unexpected reap ack: {ack!r}"
        print("   grandchild reaped by its real parent (the forked child), not by this harness.")
    finally:
        # Ask the child to exit and clean up after itself (it reaps any
        # grandchild it still owns in its own finally) before forcing
        # anything -- a hard SIGKILL here, with no window, would race past
        # that cleanup and strand a still-running grandchild for up to 15s.
        try:
            os.write(cmd_w, b"exit\n")
        except OSError:
            pass
        os.close(cmd_w)
        os.close(rdy_r)
        reaped = False
        deadline = time.time() + 3.0
        while time.time() < deadline:
            done_pid, _ = os.waitpid(child_pid, os.WNOHANG)
            if done_pid == child_pid:
                reaped = True
                break
            time.sleep(0.05)
        if not reaped:
            try:
                os.kill(child_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(child_pid, 0)  # this harness IS child_pid's real parent


def main() -> None:
    failures = []
    for check in (basic_set_clear_check, fork_inheritance_check):
        try:
            check()
        except Exception as e:
            failures.append((check.__name__, e))
            print(f"CHECK FAILED ({check.__name__}): {e!r}")
    if failures:
        print(f"\n{len(failures)} check(s) failed.")
        sys.exit(1)
    print("\nall checks passed.")


if __name__ == "__main__":
    if sys.platform != "darwin":
        sys.exit("this spike only runs on macOS")
    main()
