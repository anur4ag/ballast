# Platforms and manual installation

Release binaries support macOS 11+ on Apple Silicon and Intel, and Linux amd64/arm64 on Ubuntu 22.04 or Debian 12 and newer.
Linux binaries are built on Ubuntu 22.04 against glibc 2.35; Debian 12 provides glibc 2.36.
No root daemon is required.
Linux needs a working systemd user session.
Older systemd versions ignore the optional restart-backoff settings and retain the one-second restart delay.

For a manual install, download your platform archive and `SHA256SUMS` from the same [release](https://github.com/anur4ag/ballast/releases).
Verify the archive's checksum, extract it, place `ballast` in a permanent directory on your PATH, then run `ballast install`.
On macOS use `shasum -a 256 -c SHA256SUMS`; on Linux use `sha256sum -c SHA256SUMS` (unavailable files in the combined checksum file are reported separately).
Archives are not Apple-notarized; macOS may require approval for a downloaded executable.

Hooks and the user service retain the invoked binary path, including Homebrew's stable symlink.
For manual upgrades, replace that file atomically with the new binary.
The daemon notices replacement after finishing a tick and exits so the service manager starts the new version.
Do not run installation directly from a versioned Homebrew Cellar path.
If you move an installation, rerun `ballast install` from the new path and approve changed Codex hooks again.

`ballast doctor` reports Homebrew, APT or manual installation, checks service and hook paths, and reports missing or changed Codex approval.
It does not approve hooks on your behalf.
Project or managed agent settings can override user-level configuration.
Desktop notifications use `osascript` on macOS and `notify-send` on Linux when available.
A successful notification submission does not prove visible desktop delivery.

For isolated environments, use absolute `HOME`, `BALLAST_HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `BALLAST_SERVICE_DIR`, and a unique `BALLAST_SERVICE_LABEL`.
Linux also respects `XDG_CONFIG_HOME`.
Keep the Ballast home short enough for a Unix socket path (under 104 bytes on macOS and 108 on Linux, including `/run/ballastd.sock`).

CPU and I/O throttling is supported on macOS through reversible external DARWIN_BG policy.
Foreground improvements and the I/O trigger remain subject to calibration.
`ballast doctor` reports throttling unsupported on Linux in v0.1.
Migrating a running Linux workload to a transient scope changes its original unit lifetime, and unprivileged nice cannot be restored with the default limit.
Linux CPU/I/O pressure does not trigger a Ballast action.
