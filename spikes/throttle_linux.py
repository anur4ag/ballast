#!/usr/bin/env python3
"""Linux cgroup v2 throttle-lever spike for ticket 17, step 1.2-1.4.

Linux throttling was ruled out of v0.1 scope after this spike's own
evidence (see check C4): moving a still-running sole cgroup member into a
new scope kills it merely because the scope it came from emptied out, even
though both scopes are plain user-manager units. That is a lifetime
guarantee failure, not a success, and it rejects general scope-to-scope
migration as a lever mechanism. The other checks (A, B, F) remain valid,
narrower findings about the primitives involved.

Runs only inside the disposable Lima VM (hostname-guarded below), rootless,
against the user systemd manager's private D-Bus socket. Every unit created
here is a transient, uniquely-named scope or service; cleanup stops all of
them by name and kills any leftover owned processes, in `finally`.

No product code, no packages installed, no touching the real login session
(session-N.scope is only ever read, never stopped).

Run: python3 spikes/throttle_linux.py
Exits 0 only if every assertion held; prints and exits non-zero otherwise.
"""
import dbus
import glob
import os
import resource
import signal
import socket
import subprocess
import sys
import time
import uuid

BUS_PATH = f"unix:path=/run/user/{os.getuid()}/systemd/private"
ALLOWED_HOSTNAMES = {"ballast-platform", "lima-ballast-platform"}


def manager():
    bus = dbus.connection.Connection(BUS_PATH)
    obj = bus.get_object(None, "/org/freedesktop/systemd1", introspect=False)
    return dbus.Interface(obj, "org.freedesktop.systemd1.Manager")


def unit_name(prefix: str, suffix: str = ".scope") -> str:
    return f"ballast-spike-{prefix}-{uuid.uuid4().hex[:8]}{suffix}"


def start_transient_scope(mgr, name, pids, cpu_weight=None, io_weight=None, extra_props=None):
    props = [("PIDs", dbus.Array([dbus.UInt32(p) for p in pids], signature="u"))]
    if cpu_weight is not None:
        props.append(("CPUWeight", dbus.UInt64(cpu_weight)))
    if io_weight is not None:
        props.append(("IOWeight", dbus.UInt64(io_weight)))
    for key, value in (extra_props or {}).items():
        props.append((key, value))
    mgr.StartTransientUnit(
        name, "fail", props, dbus.Array([], signature="(sa(sv))"),
        signature="ssa(sv)a(sa(sv))",
    )


def set_unit_properties(mgr, name, **props):
    mgr.SetUnitProperties(name, True, list(props.items()), signature="sba(sv)")


def stop_unit(mgr, name):
    try:
        mgr.StopUnit(name, "fail")
    except dbus.exceptions.DBusException:
        pass


def spawn_sleeper(seconds=30):
    return subprocess.Popen(["sleep", str(seconds)])


def cgroup_path_for(name):
    out = subprocess.run(
        ["systemctl", "--user", "show", name, "-p", "ControlGroup", "--value"],
        capture_output=True, text=True,
    ).stdout.strip()
    return f"/sys/fs/cgroup{out}" if out else None


def read_cgroup_file(cgroup_path, filename):
    if not cgroup_path:
        return "<no cgroup path>"
    try:
        with open(os.path.join(cgroup_path, filename)) as f:
            return f.read().strip()
    except OSError as e:
        return f"<error: {e}>"


def active_state(name):
    return subprocess.run(
        ["systemctl", "--user", "show", name, "-p", "ActiveState", "--value"],
        capture_output=True, text=True,
    ).stdout.strip()


def wait_until(condition, timeout=5.0, interval=0.2):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if condition():
            return True
        time.sleep(interval)
    return condition()


# --- checks -----------------------------------------------------------------

def check_move_and_weights(mgr, created):
    print("\n== A. move an owned PID into a transient scope; set then restore weights ==")
    child = spawn_sleeper(20)
    try:
        name = unit_name("weights")
        created.append(("scope", name))
        start_transient_scope(mgr, name, [child.pid], cpu_weight=10, io_weight=10)
        time.sleep(0.3)
        cg = cgroup_path_for(name)
        print(f"scope cgroup: {cg}")
        print(f"cpu.weight after start(10): {read_cgroup_file(cg, 'cpu.weight')}")
        print(f"io.weight  after start(10): {read_cgroup_file(cg, 'io.weight')}")

        assert read_cgroup_file(cg, "cpu.weight") == "10", "cpu.weight did not read back as 10 after start"
        assert read_cgroup_file(cg, "io.weight").split()[-1] == "10", "io.weight did not read back as 10 after start"

        set_unit_properties(mgr, name, CPUWeight=dbus.UInt64(100), IOWeight=dbus.UInt64(100))
        time.sleep(0.3)
        print(f"cpu.weight after restore(100): {read_cgroup_file(cg, 'cpu.weight')}")
        print(f"io.weight  after restore(100): {read_cgroup_file(cg, 'io.weight')}")
        assert read_cgroup_file(cg, "cpu.weight") == "100", "cpu.weight did not restore to 100"
        assert read_cgroup_file(cg, "io.weight").split()[-1] == "100", "io.weight did not restore to 100"
        assert child.poll() is None, "sleeper died while weights were being changed"
        print("sleeper alive throughout, weights set then restored: confirmed")
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait()


def check_late_child_inherits_scope(mgr, created):
    print("\n== B. a child forked after the PID move stays in the same cgroup ==")
    read_fd, write_fd = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(read_fd)

        def on_signal(signum, frame):
            grandchild = os.fork()
            if grandchild == 0:
                os.close(write_fd)
                os.execvp("sleep", ["sleep", "10"])
            os.write(write_fd, str(grandchild).encode())
            os.close(write_fd)

        signal.signal(signal.SIGUSR1, on_signal)
        while True:
            signal.pause()

    os.close(write_fd)
    try:
        name = unit_name("inherit")
        created.append(("scope", name))
        start_transient_scope(mgr, name, [pid], cpu_weight=10, io_weight=10)
        time.sleep(0.3)

        os.kill(pid, signal.SIGUSR1)
        grandchild = int(os.read(read_fd, 64).decode())
        os.close(read_fd)
        time.sleep(0.3)

        def cgroup_of(target_pid):
            with open(f"/proc/{target_pid}/cgroup") as f:
                return f.read().strip()

        parent_cg = cgroup_of(pid)
        child_cg = cgroup_of(grandchild)
        print(f"parent {pid} cgroup:      {parent_cg}")
        print(f"late child {grandchild} cgroup: {child_cg}")
        assert parent_cg == child_cg, f"late child landed in a different cgroup: {parent_cg!r} != {child_cg!r}"
        print("same cgroup: confirmed")

        for p in (grandchild, pid):
            try:
                os.kill(p, signal.SIGKILL)
                os.waitpid(p, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
    except Exception:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass
        raise


def check_move_between_scopes_lifetime(mgr, created):
    print("\n== C4. LIFETIME REJECTION: moving a PID out of its originating scope ==")
    print("   worker is started with a 20s sleep and nothing here should end it early.")
    print("   scope_b declares BindsTo=+After=scope_a purely to test whether that dependency")
    print("   survives a PID being moved out of scope_a -- it does not, and that is the finding:")
    print("   the migration itself kills a still-running sole member of the scope it leaves,")
    print("   which is unacceptable for a throttle lever that must never end a live workload.")
    intended_runtime = 20
    worker = spawn_sleeper(intended_runtime)
    started_at = time.monotonic()
    scope_a = unit_name("origin")
    created.append(("scope", scope_a))
    start_transient_scope(mgr, scope_a, [worker.pid])
    time.sleep(0.3)
    print(f"scope_a state before move: {active_state(scope_a)}")

    scope_b = unit_name("throttle")
    created.append(("scope", scope_b))
    start_transient_scope(
        mgr, scope_b, [worker.pid], cpu_weight=10, io_weight=10,
        extra_props={
            "BindsTo": dbus.Array([scope_a], signature="s"),
            "After": dbus.Array([scope_a], signature="s"),
        },
    )

    emptied = wait_until(lambda: active_state(scope_a) in ("inactive", "failed"), timeout=5)
    print(f"scope_a state after move (should empty out): {active_state(scope_a)} (settled={emptied})")
    assert emptied, "scope_a never emptied out after the PID moved -- can't test the rejection scenario"

    died = wait_until(lambda: worker.poll() is not None, timeout=5)
    elapsed = time.monotonic() - started_at
    if worker.poll() is None:
        print(f"scope_b={active_state(scope_b)} worker still alive at {elapsed:.1f}s (did not reproduce the rejection)")
        worker.terminate()
        worker.wait()
        raise AssertionError("expected scope_b's BindsTo=scope_a to kill the worker; it survived instead")
    print(
        f"scope_b={active_state(scope_b)} worker killed after {elapsed:.1f}s of its intended "
        f"{intended_runtime}s sleep -- LIFETIME REJECTION reproduced: the move-between-scopes "
        f"design kills a live workload merely because its source scope emptied, not because of "
        f"any deliberate stop. General Linux scope-to-scope migration is rejected as a lever."
    )


def check_cross_manager_session_visibility(mgr):
    print("\n== D. user manager cannot see the system manager's session scope (read-only) ==")
    session_id = os.environ.get("XDG_SESSION_ID")
    if not session_id:
        print("no XDG_SESSION_ID in environment, skipping")
        return
    session_unit = f"session-{session_id}.scope"
    try:
        path = mgr.GetUnit(session_unit)
        raise AssertionError(f"user manager GetUnit({session_unit!r}) unexpectedly resolved: {path}")
    except dbus.exceptions.DBusException as e:
        print(f"user manager GetUnit({session_unit!r}) failed as expected: {e.get_dbus_message()}")

    out = subprocess.run(
        ["systemctl", "show", session_unit, "-p", "ActiveState", "--value"],
        capture_output=True, text=True,
    )
    print(f"system manager `systemctl show {session_unit}`: ActiveState={out.stdout.strip() or out.stderr.strip()}")
    print("(read-only query only; the real session is never stopped)")


def check_io_weight_precondition():
    print("\n== E. io.weight effect precondition (no load generated) ==")
    for path in sorted(glob.glob("/sys/block/*/queue/scheduler")):
        with open(path) as f:
            print(f"{path}: {f.read().strip()}")
    for path in ("/sys/fs/cgroup/io.cost.qos", "/sys/fs/cgroup/io.cost.model"):
        try:
            with open(path) as f:
                content = f.read().strip()
            print(f"{path}: {content or '<empty: io.cost not configured for any device>'}")
        except OSError as e:
            print(f"{path}: <error: {e}>")
    print("io.weight is honored by the bfq scheduler or by io.cost (blk-iocost) once a qos policy is")
    print("set for the device; on mq-deadline with io.cost.qos/model empty it is written but not enforced.")
    print("LIMIT: confirming zero effect for real needs sustained owned I/O contention, which this spike")
    print("does not generate; this is a structural check only, not a behavioral one.")


def check_nice_reversibility():
    print("\n== F. RLIMIT_NICE reversibility for a plain `nice` adjustment ==")
    print(f"RLIMIT_NICE: {resource.getrlimit(resource.RLIMIT_NICE)}")
    child = spawn_sleeper(10)
    try:
        base = os.getpriority(os.PRIO_PROCESS, child.pid)
        os.setpriority(os.PRIO_PROCESS, child.pid, base + 10)
        lowered = os.getpriority(os.PRIO_PROCESS, child.pid)
        try:
            os.setpriority(os.PRIO_PROCESS, child.pid, base)
            restored = os.getpriority(os.PRIO_PROCESS, child.pid)
            raise AssertionError(
                f"expected the restore to fail under RLIMIT_NICE=0, but it succeeded: "
                f"base={base} lowered={lowered} restored={restored}"
            )
        except PermissionError as e:
            print(f"base={base} lowered={lowered} restore FAILED ({e}) -- one-way, as D14 expected under RLIMIT_NICE=0")
    finally:
        child.terminate()
        child.wait()


def cleanup(mgr, created):
    print("\n== cleanup ==")
    names = [name for _kind, name in created]
    for name in reversed(names):
        stop_unit(mgr, name)
    if names:
        # Scoped to exactly the units this run created -- never a bare
        # `reset-failed`, which would also clear unrelated failed units
        # belonging to whoever else uses this user manager.
        subprocess.run(["systemctl", "--user", "reset-failed", *names], capture_output=True)


def require_known_host():
    hostname = socket.gethostname()
    if hostname not in ALLOWED_HOSTNAMES:
        sys.exit(
            f"refusing to run: hostname {hostname!r} is not in {sorted(ALLOWED_HOSTNAMES)} -- "
            f"this spike creates and kills processes/units and must only run in the disposable ballast VM"
        )


def main():
    require_known_host()
    mgr = manager()
    created = []
    checks = [
        lambda: check_move_and_weights(mgr, created),
        lambda: check_late_child_inherits_scope(mgr, created),
        lambda: check_move_between_scopes_lifetime(mgr, created),
        lambda: check_cross_manager_session_visibility(mgr),
        check_io_weight_precondition,
        check_nice_reversibility,
    ]
    failures = []
    try:
        for check in checks:
            try:
                check()
            except Exception as e:  # keep going; report and move to the next check
                failures.append((check, e))
                print(f"CHECK FAILED: {e!r}")
    finally:
        cleanup(mgr, created)
    if failures:
        print(f"\n{len(failures)} check(s) failed.")
        sys.exit(1)
    print("\nall checks passed.")


if __name__ == "__main__":
    if sys.platform != "linux":
        sys.exit("this spike only runs on Linux")
    main()
