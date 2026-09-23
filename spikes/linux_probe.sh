#!/bin/sh
# Runs only inside the disposable ballast-spike VM.
set -eu
unit=ballast-verification-pressure
cleanup() {
  systemctl --user stop "$unit.service" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM
uname -a
cat /proc/pressure/memory
systemd-run --user --unit="$unit" --collect --property=MemoryHigh=96M --property=MemoryMax=256M --property=MemorySwapMax=0 --property=RuntimeMaxSec=45 /usr/bin/python3 -c 'import time; chunks=[bytearray(8*1024*1024) for _ in range(20)]; time.sleep(30)'
i=0
while [ "$i" -lt 45 ]; do
  date +%s
  cat /proc/pressure/memory
  scope=$(systemctl --user show "$unit.service" -p ControlGroup --value 2>/dev/null || true)
  if [ -n "$scope" ] && [ -f "/sys/fs/cgroup$scope/memory.pressure" ]; then
    cat "/sys/fs/cgroup$scope/memory.pressure"
    cat "/sys/fs/cgroup$scope/memory.events"
  fi
  sleep 1
  i=$((i+1))
done
cleanup
printf 'Notification service probe\n'
printf 'DISPLAY=%s WAYLAND_DISPLAY=%s\n' "${DISPLAY:-unset}" "${WAYLAND_DISPLAY:-unset}"
export DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$(id -u)/bus"
systemd-run --user --wait --pipe --collect --unit=ballast-verification-notification --property=RuntimeMaxSec=10 /usr/bin/notify-send 'Ballast verification' 'Temporary Linux service notification probe' || true
