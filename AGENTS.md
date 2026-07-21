# Repository Guidelines

## Project Structure & Module Organization

EzMusic is a single Rust package. src/main.rs owns CLI dispatch, while src/lib.rs
exports the testable modules. Keep persistence in src/db.rs, source integration in
src/source.rs, downloads in src/download.rs, real-time audio in src/player.rs,
and terminal behavior in src/tui.rs. Shared bounded-process helpers live in
src/process.rs. Integration tests belong in tests/, operational design in docs/,
and CI in .github/workflows/. Never commit target/, downloaded tools, audio files,
SQLite files, or partial downloads.

## Build, Test, and Development Commands

- `cargo run --release --locked`: open the optimized TUI.
- `cargo test --locked`: run unit and subprocess integration tests.
- `cargo fmt --check`: verify Rust formatting.
- `cargo clippy --all-targets --locked -- -D warnings`: reject lint warnings.
- `cargo build --release --locked`: produce the release binary.
- `cargo run --release --locked -- benchmark PATH`: measure RSS, CPU, and underruns.

Linux release builds require pkg-config, libasound2-dev, and CMake. Rust 1.89 is pinned in
rust-toolchain.toml. Repository Cargo configuration intentionally uses one build job;
do not raise it without considering memory pressure.

## Coding Style & Naming Conventions

Use rustfmt defaults with the repository’s 100-column limit. Names follow Rust
conventions: snake_case functions/modules, PascalCase types, and
SCREAMING_SNAKE_CASE constants. Keep audio callbacks allocation-free, lock-free,
and free of I/O. Pass external arguments through std::process::Command; never build
shell command strings from search results or URLs. Preserve all queue, output-size,
timeout, subprocess-cleanup, and input limits when extending online features.

## Testing Guidelines

Place focused unit tests beside their modules and cross-module scenarios in tests/.
Name tests by behavior, such as marks_missing_imports_unavailable. Network-dependent
YouTube checks must not block pull requests; use fake yt-dlp/FFmpeg executables for
deterministic CI. Every audio change must cover queue/seek/error behavior and run the
release benchmark against an Opus fixture. Tests must remain offline and resource-bounded.

## Commit & Pull Request Guidelines

This checkout has no Git history to infer an established convention. Use imperative
Conventional Commits, for example feat: add playlist resolver or fix: avoid allocation
in audio callback. Pull requests must describe user-visible behavior, list commands
run, and report benchmark deltas for player/audio changes.
Link issues and include terminal recordings when they materially clarify a TUI
change. Keep migrations backward-compatible and call out tool, schema, or config
changes explicitly.

## Security & Configuration

Verify downloaded tool hashes before activation and retain atomic rollback. Do not
add cookies, credentials, DRM bypasses, telemetry, or arbitrary command execution.
Downloaded content remains the user’s legal responsibility.
