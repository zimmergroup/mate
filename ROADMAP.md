# Roadmap

Status reflects implemented and locally verified behavior, not planned architecture.

## Phase 1 - Foundation

- DONE: Stable Rust crate, CLI help/version, logging, configuration model and platform config path.
- DONE: Ratatui/Crossterm terminal lifecycle, dark theme, responsive shell, keyboard quit/help.
- DONE: Topic-store latest-value model and MQTT wildcard-filter validation tests.
- DONE: Create plaintext broker profiles in the UI and atomically persist TOML.
- DONE (0.0.3): Duplicate profiles and set/clear the default profile in the TUI (`u`, `*`) and CLI (`broker duplicate`, `broker default`) through the shared store. Imported certificates are not copied to the duplicate.

## Per-broker configuration and certificates

- DONE: Stable broker IDs, per-broker `broker.toml`, legacy root-profile migration, atomic profile writes, and broker-relative path resolution.
- DONE: CLI broker list/add/edit/remove/test and profile import/export through shared services.
- DONE: PEM imports and external references, expiry/metadata reporting, safe managed-file removal, Unix private-key mode, and Windows ACL implementation.
- DONE: Sensitive certificate bundle export requires explicit confirmation; private-key inclusion is separately gated.
- DONE: TUI broker list/add/edit/confirmed-delete and certificate metadata/import/reference/confirmed-remove flows use the shared services.
- DONE: Verified on Linux against Mosquitto 2.1.2: plain, TLS (custom CA) and mTLS connect and receive a retained message; wrong CA and missing client certificate are rejected.
- DONE: Keyring password store/lookup via `mate broker password` verified on Linux; authenticated connect to a real broker and topic browsing with JSON payloads worked.
- DONE: TUI broker editor accepts username and password; password storage was exercised through the same code path as the CLI against the real keyring (stored, looked up, removed again).
- DONE: Verified on Linux against a password-protected Mosquitto 2: a wrong password stops after one connection attempt with "Authentication failed ... not retrying" (CLI and TUI), the correct password connects, and after a broker restart the TUI shows Reconnecting, reconnects, resubscribes and receives new messages.
- DONE: mTLS connection to a production broker (MQTT 3.1.1, imported certificates, credentials) confirmed by the user; topics and payloads were browsed in the TUI. Read-only use, no publishing.
- TODO: Verify TLS handshakes with an expired certificate against a real broker (expiry is covered by unit tests only).
- DONE: Windows x86_64 (GNU) cross-build produces `mate.exe` in CI; it links against system DLLs only, starts under Wine (`--version`, broker add/list) and was confirmed to run in a Windows VM (QEMU) by the maintainer.
- TODO: Windows ACL behavior verification; real TLS/mTLS broker tests on Windows.

## Phase 2 - Connectivity

- DONE: MQTT 3.1.1 and MQTT 5 connections behind one protocol-agnostic event loop, several saved subscriptions per broker (enable/disable, QoS), live subscribe/unsubscribe, bounded message delivery, clean shutdown, and exponential reconnect delay. Verified against a local Mosquitto 2 for both protocol versions (integration tests and a scripted TUI session); not verified against other production brokers.
- DONE: MQTT 5 message properties (content type, user properties, expiry, response topic, correlation data) in the topic metadata view (`i`); verified with an integration test against Mosquitto 2. DONE: searching by property in the history filter (`/` in the history panel).
- DONE: Rustls TLS/mTLS profiles, system/custom CA validation (verified on Linux, see above).
- DONE: Unique client ID per connection (random suffix, can be switched off for brokers with fixed client IDs) so several instances can share a profile; verified against Mosquitto.
- TODO: Precise auth retry policy beyond stopping on refused credentials.
- DONE: Windows GNU cross-build runs in CI and keeps `mate.exe` as an artifact.

## Phase 3 - Topic Explorer

- DONE: Hierarchical topic rows, expand/collapse, path-stable selection, and payload inspection by selected topic. Payload-only updates do not invalidate the structural tree cache.
- DONE (0.0.3): compact one-line previews, activity dots from message timestamps, incremental case-insensitive/fuzzy topic filter with highlighting, favorites (saved per broker) with a favorites-only filter, viewport-only row rendering. Checked with a scripted pseudo-terminal session and a 300,000-message flood over 5,000 topics against a local Mosquitto (UI stayed responsive; overflowing messages are counted as dropped).
- DONE: Per-subtree message and topic counts in the metadata view (`i` on a group); not shown in the topic rows themselves.

## Phase 4 - Inspection

- DONE (0.0.3): interactive JSON tree (collapse/expand, paths, search, horizontal scroll), raw and hex views, line diff, bounded per-topic history with stable historical selection and live/history indicator, optional history panel.
- DONE: copy of values, payloads and topics (`c`, `C`, `t`, `Ctrl+C`) through `wl-copy`/`xclip`/`xsel` with OSC 52 fallback; verified with `wl-copy` on Wayland. The OSC 52 fallback is not verified in a real terminal.
- DONE: history search (`/` in the history panel) over payload text and MQTT 5 properties.
- TODO: Semantic (structural) JSON diff; export of history.

## Subscriptions

- DONE: System topics (`$SYS/#`) as a per-broker option (`y` in the broker manager); verified against a local Mosquitto.
- DONE: Exclusions (topic filters whose messages are dropped on arrival, `x` in the subscription manager, `broker exclude`); verified against a local Mosquitto, applied live. TODO: remove already received topics when an exclusion is added.

## Phase 4c - Stream and docked publishing

- DONE: Stream view (`S`): bounded chronological log of messages and connection events with expandable details, pause, topic and payload/property filters; covered by unit and `TestBackend` tests.
- DONE: Publish panel (`P`) docked below the panes, reusing the dialog's editor, review and history; the dialog remains.
- TODO: Follow the explorer's selection with the panel's topic; maximizing the panel on small terminals.

## Phase 4a - Analytics

- DONE: Analytics view (`A`) with seven lists: frequency, not-valid-JSON, quiet/overdue, jitter, largest payloads, retained topics and branch totals; covered by unit and `TestBackend` tests, not yet run against a real PLC. Topic filter (`/`) and pause (`Space`) are DONE.

## Phase 4b - Layout and status (0.0.3)

- DONE: two/three-pane layouts, maximizing, adjustable widths (saved in `ui-state.toml`), narrow-terminal fallback, status bar with one authoritative connection state, context-sensitive shortcut bar, broker table with details and connection test.
- TODO: Run the TUI on Windows Terminal and verify key handling (Ctrl+arrows, OSC 52, credential store) there; needs a Windows machine or runner.

## Phase 5 - Publishing

- DONE: Publish dialog (`p`) with review step, QoS and retain, warnings for retained and retained-delete (empty payload) messages, bracketed paste for multi-line payloads. Read-only by default: a profile must opt in (`allow_publish`, `w` in the broker manager or `broker edit --allow-publish true`), and the MQTT runtime refuses to publish otherwise, whatever the front end. Verified end to end against a local Mosquitto (TUI session with paste and retained message, plus an integration test); not verified against a production broker.
- DONE: Publish history: the dialog lists the last 20 sent messages per broker below the fields; they can be loaded, edited and sent again.
- DONE: payload from a UTF-8 text file (`Ctrl+O` in the payload field).
- TODO: Publish templates, binary payload files, confirmation of the permission change.

## Phases 6-7 - Advanced features and release

- DONE (0.1.0): MIT license, CI on every commit (fmt, clippy, tests, coverage, Linux build, Windows cross-check) and a tag-triggered release job that publishes a Linux x86_64 archive with checksum and creates a release. Verified with `v0.1.0`: archive checksum, binary start and `cargo install --git ... --tag v0.1.0 --locked`.
- DONE (0.3.0): the Windows archive is built by the pipeline and published with every tag; the release text is generated from the matching `CHANGELOG.md` section (the job fails if it is missing).
- DONE: `--connect`, `auto_connect`, terminal title, merged `ui-state.toml` writes for several instances. DONE: several brokers connected at once in one merged tree/stream with origin markers, per-broker `.mqttlog` files rotated by day and size, replay of recordings to any broker. TODO: Watchlists, statistics, separate per-broker tabs, mouse support, performance benchmarks, binaries for other platforms.

## Ideas, not yet scheduled

Small additions that fit the existing views; none of them needs a new architecture.

- Export an analytics list or a topic's history as CSV or JSON lines (file or clipboard).
- Compare two analytics snapshots: new, vanished and strongly changed topics.
- Expected rate or maximum silence per topic, flagged in the quiet list (no notifications).
- Subscription help: warn about `#` on a busy broker and create a filter from the selected topic.
- JSON schema drift: notice when the keys of a topic's payload change.
- Semantic JSON diff that ignores key order.
- Publish templates.
- Configurable colors and key bindings.
