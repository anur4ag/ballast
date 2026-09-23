# Ballast

Keep your machine responsive while coding agents work. Early development, macOS and Linux only.

```sh
cargo run -- debug platform
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`debug platform` prints raw process and pressure data as JSON. CPU time is cumulative
nanoseconds, memory is bytes, and swap/pageout counters count pages. Environment is
read on demand and never included in debug output. Other subcommands are placeholders.

The platform API lives in `src/platform`. Unknown readings are `None`, including
inaccessible processes and unreadable or truncated environments. Linux memory is
RSS; macOS memory is physical footprint. Process identities combine PID and an
opaque OS start-time token. Persisted identities must also carry `boot_id()` and
be discarded after a reboot. Linux uses pidfds where available; macOS revalidates
the identity immediately before `kill`, with the small residual PID reuse race
inherent in that API. Ports are queried separately from the fast scan.

Building on macOS requires the Xcode Command Line Tools for the small socket-info
C shim. Runtime dependencies are the system APIs; notifications use `osascript`
on macOS and `notify-send` when available on Linux. Successful invocation does not
guarantee notification delivery by the desktop session.
