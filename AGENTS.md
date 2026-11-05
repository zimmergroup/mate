# Agent Guide

## Architecture

- `main.rs` owns CLI startup, tracing, and terminal lifecycle.
- `config.rs` owns the serialized settings model and root config discovery/loading.
- `broker.rs` owns stable broker IDs, `brokers/<id>/broker.toml` persistence, legacy migration, and broker-relative path resolution. CLI and TUI must use this service.
- `certificates.rs` owns certificate import/reference/removal, validation, metadata, and explicit certificate bundle export. Never duplicate these operations in command or UI handlers.
- `security.rs` owns OS-keyring lookup and Rustls configuration. It must resolve file paths through `broker.rs` and must never disable verification.
- `topics.rs` owns topic state, the incrementally-maintained hierarchy, visible rows (optionally filtered), bounded per-topic message history with its global byte/message budget, and MQTT topic-filter matching; it must not depend on the UI. Payload-only updates must not rebuild the structural row cache.
- `analytics.rs` (payload classification, message-rate arithmetic), `mqttlog.rs` (the `.mqttlog` line format, its parser and the per-broker file writer with day/size rotation), `replay.rs` (loading recordings, pacing and the play/pause state machine), `search.rs` (substring search over payload and MQTT 5 properties), `snake.rs` (the rules of the help overlay's hidden snake game; input/timing in `ui/help.rs`, drawing in `ui/render.rs`), `stream.rs` (the bounded message/event log of the stream view), `json_tree.rs`, `preview.rs`, `diff.rs`, `fuzzy.rs`, `stats.rs` are pure helpers (JSON tree rows/paths/search, one-line topic previews, line diff, match positions, message-rate window). They must not depend on Ratatui. `clipboard.rs` is a helper too: base64/OSC 52 plus `copy_native` (`wl-copy`/`xclip`/`xsel`); only `main.rs` calls it, `UiState` just queues a `ClipboardRequest`.
- `ui_state.rs` owns `ui-state.toml` (layout, favorites). UI preferences must not go into `broker.toml`.
- `mqtt.rs` owns MQTT I/O for MQTT 3.1.1 and MQTT 5 behind one protocol-agnostic loop. It reports `BrokerEvent`s (`State`, `Notice`, `Failed`, `Message`) over a bounded channel; `ConnectionState` is the only vocabulary for connection status. `test_connection` is shared by the CLI and the TUI. Publishing goes through `BrokerRuntime::publish`, which enforces the profile's `allow_publish` and validates the request; front ends must not bypass it. Subscriptions are persisted through `BrokerStore` and applied to a running connection with `BrokerRuntime::subscribe/unsubscribe`.
- `ui.rs` holds `UiState` (the authoritative `ConnectionInfo` per broker in `connections`, the one shared topic store, dialogs) and input handling; `ui/layout.rs` is pure layout math, `ui/main_view.rs` the main view's state and keys, `ui/analytics.rs` the analytics view's state, row computation and keys (frequency ranking, not-valid-JSON list), `ui/help.rs` the help overlay's tabs, content data and keys, `ui/logging.rs` the logging dialog, `ui/replay.rs` the replay menu (state, keys, batches for the event loop), `ui/inspector.rs` the parsed-payload cache and inspector modes (no drawing), `ui/stream.rs` the stream view's state, filters, detail lines and keys, `ui/render.rs` (and its submodule `ui/render/stream.rs`) all drawing, `ui/subscriptions.rs` the subscription manager, `ui/theme.rs` semantic ANSI colors. Rendering reads state only; it never touches MQTT. Do not duplicate connection state outside `UiState::connections`. Every message carries the ID of the broker that delivered it (`Message::broker`); anything that talks to one broker (publish, live subscription changes) is addressed by broker ID in the `UiAction`.
- `main.rs` owns CLI startup, tracing, terminal lifecycle and the event loop that tags each broker's `BrokerEvent`s with its ID and a generation (events of a replaced connection are dropped), moves them into `UiState`, spawns/stops one broker task per connection for `UiAction`s, appends messages to the `.mqttlog` files when `UiState::log_directory` says so, publishes the batches `UiState::replay_batch` hands out, and writes clipboard data (system tool, OSC 52 fallback).
- MQTT I/O belongs in its own module and must communicate with the UI through bounded channels. Never do payload formatting or rendering in the MQTT event loop.

Keep domain types independent of Ratatui. Store usernames and keyring references, never passwords or private-key contents, in TOML. Imported private keys must use platform-specific secure permissions; do not assume POSIX permission bits on Windows. Do not follow symlinks during certificate imports (links owned by root, such as `/var` on macOS, are allowed). Relative certificate paths are rooted at the profile directory, never the process working directory. Avoid platform-specific assumptions in shared code.

## Conventions

- Between releases `Cargo.toml` carries a development version (`0.4.0-dev.N`); raise `N` with every pushed change so the status bar shows which build runs. A release replaces it with the final version, and the release job only accepts final `vX.Y.Z` tags.
- Use stable Rust and preserve cross-platform compilation.
- Prefer explicit errors and small modules with focused tests.
- Bound queues and history; account for any dropped messages.
- Keyboard shortcuts live in three places that must change together: the handler, `shortcut_items`/`render_help` in `ui/render.rs`, and the README key table. Never show a shortcut in the bar that is not implemented.
- UI behavior is tested without a terminal: drive `UiState` with `handle_key`/`apply_broker_event` and assert on `TestBackend` screens (see the tests in `ui.rs`).
- Add functionality only when it works end to end, and update `ROADMAP.md` and `CHANGELOG.md` to match verified behavior.

## Validation

Run before proposing changes:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
cargo deny check   # licenses and advisories, if cargo-deny is installed
cargo check --target x86_64-pc-windows-gnu
```

The Windows cross-check requires the Windows GNU Rust target and MinGW-w64 compiler; CI builds `mate.exe` the same way (`cargo build --release --target x86_64-pc-windows-gnu`).

The MQTT integration tests (`tests/broker_integration.rs`) need a real Mosquitto and are skipped unless `MATE_TEST_BROKER_HOST` is set; see the file header for the other `MATE_TEST_*` variables (plain/TLS/mTLS ports, CA, client certificate and key). A normal build and unit tests must not require Docker or a broker.