# Contributing

Install Rust 1.97.1 and, on macOS, the Xcode Command Line Tools for the socket-info C shim.

```sh
cargo build --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

CI runs formatting, strict clippy and the complete default test suite on macOS and Linux.
Opt-in process/service fixtures are marked ignored and describe their requirements.
Tests use temporary homes with `notifications = false`.
Never run install tests against your real agent configuration or service label.

Use `cargo run -- debug platform` to inspect raw process and pressure data as JSON.
CPU time is cumulative nanoseconds, memory is bytes, and pageout/swapout counters count pages.
Environment is read on demand and is not printed by that command.

Run `cargo run -- daemon` in an isolated `BALLAST_HOME` for foreground debugging, then use `status`, `ps` or `top` from another terminal with the same home.
[Reproduce the terminal fixture](cli.md#reproduce-the-terminal-check) for light/dark captures.
