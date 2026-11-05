# Changelog

All notable changes to MATE are recorded here. Entries describe behavior that is implemented and tested; see [ROADMAP.md](ROADMAP.md) for what is still open.

## Unreleased

### Changed

- Fixed on macOS: certificate import and export refused paths below `/var` or `/tmp` (for example the temporary directory) as "traverses a symlink". Symlinks owned by root are now accepted as parts of a path; a symlink created by the user is still refused.
- Fixed on Windows: `mate broker export-certificates` failed with "Access is denied" because the bundle directory was renamed while its `broker.toml` was still open.
- The copyright holder named in `LICENSE` is Zimmer Group; the README introduces MATE with its full name.
- The project moved to GitHub; CI and releases run on GitHub Actions (formatting, Clippy, tests on Linux, Windows and macOS, a Linux release build and a Windows GNU cross-build). Tests and documentation use invented names instead of names from real installations.

### Added

- Release archives contain `THIRD-PARTY-LICENSES.txt` (licenses of all built-in crates, generated with `cargo about`). CI checks dependency licenses and security advisories with `cargo deny`; the advisories that do not affect MATE are listed with reasons in `deny.toml`.
- `SECURITY.md`, `CONTRIBUTING.md` and Dependabot for Cargo and GitHub Actions.
- `demo/demo.sh`: two Mosquitto brokers in Docker (plain MQTT 3.1.1 and TLS with MQTT 5) fed with invented plant data and a separate MATE configuration, for screenshots and for trying MATE out.

## 0.9.0 - 2026-11-05

### Added

- Easter egg: `6` in the help opens a hidden Snake tab, played with the arrow keys (Enter starts or pauses, Tab leaves, Esc closes the help). The game pauses whenever the tab or the help is left; the best score lasts until MATE exits.
- Ctrl+V (and Shift+Insert) pastes the system clipboard into the focused text field, for example a JSON text into the publish payload. MATE reads the clipboard itself, so it also works in terminals that pass Ctrl+V through to the program instead of pasting.

### Changed

- Popups (publish, broker, certificate, subscription, logging, confirmations, help) list their keys in a highlighted footer line inside the frame, in the style of the shortcut bar. The publish popup's plain hint lines and the inline `[y] ... [n/Esc] ...` texts are gone; the broker editor shows `←/→ Protocol` on the protocol field and the publish popup is a little wider so its footer fits.
- Renaming a broker profile moves its configuration folder to the new name (`brokers/<id>/`), in the editor and with `mate broker edit <id> --name`. Before, a duplicate called `Production copy` kept the folder `production-copy` after it was renamed. The default broker, favorites, publish history, a generated client ID and a keyring password follow the new ID. A profile with a running connection cannot be renamed.

### Fixed

- Ctrl+V in the publish editor typed a `v` in terminals that deliver the key; other unassigned Ctrl+letter keys typed their letter too. They are ignored now (AltGr, which is Ctrl+Alt on Windows, still types).
- A paste with old Mac or terminal line endings (a lone carriage return) put carriage returns into the payload; every line break becomes a new line now.

## 0.8.0 - 2026-10-29

### Added

- `p` in the payload inspector or the history uses the selected message as a template: topic, payload, QoS and retain are filled into the publish editor and the cursor stands at the start of the payload, ready to change a sequence number. The docked publish panel loads it the same way.
- `Ctrl+F` in the publish editor now switches a JSON payload between the indented form and one line; before, it could only indent.
- Stream: `C` empties the list (filters stay). The stream keeps 50,000 entries / 64 MiB instead of 5,000 / 32 MiB, so a filter finds older messages of rarer topics too.

### Fixed

- The help no longer lists the inspector keys `H` and `L`, which were removed in 0.7.0.

## 0.7.0 - 2026-10-29

### Added

- Several brokers at once: mark them with `Space` in the broker manager and press `Enter` (or `mate --connect --broker a --broker b`). They share one topic tree, history and stream; a digit per broker marks the origin in the header, in front of topics (`1·`, `·2`, `12`) and in front of stream entries. The window title lists the brokers and how many are connected. Publishing goes to the broker that delivered the selected topic (`Ctrl+B` changes it, the earlier messages are kept per broker), a resent history message to its own broker, and each broker keeps its own subscriptions, exclusions and reconnect handling.
- Logging: `L` opens a dialog that writes every received message to `.mqttlog` files, one line per message: `2026-10-23 10:53:51,998 | T0   I: <topic> [<user properties as JSON>] <payload>` (UTC, with the thread field padded like the reference logs). Files are named `<broker-id>_<YYYYMMDD>.<n>.mqttlog`: a new day and the size limit (default 20 MB, set in the dialog) start a new file, and a restart continues the newest block with room. The setting is kept in `ui-state.toml`; files are private on Unix; a write error switches logging off with a notice. Payloads are written as they are; only payloads that are not UTF-8 (`0x` and hex) or contain control characters (`\:` and escapes) change form, so every message reads back exactly.
- The log layout is configurable in `config.toml` (`[log_format]`: a template for writing, a time format, UTC or local time and an optional regular expression for reading), so recordings of other tools with a similar layout can be replayed. An unusable setting is reported and the default is used.
- Replay: `R` opens a menu to play recorded `.mqttlog` files to any connected broker that allows publishing, not only the one that recorded them. Pacing by the recorded times (speed ×0.1 to ×100) or at a fixed rate (1 to 1000 messages/s), `Space` pauses and resumes, long pauses can be capped, loop, topic filter, QoS, confirmation before the first start, `REPLAY ▶ n/m` in the header. User properties are sent on MQTT 5 connections.
- The broker a message came from is shown in the history panel, in the topic metadata and in the inspector's header line when several brokers are connected.
- `mate.exe` carries the MATE icon (Explorer, task bar, shortcuts). The Linux binary has none; a terminal program takes its window icon from the terminal.

### Changed

- `H` and `L` no longer scroll the payload inspector sideways by a double step (`L` opens the logging dialog); `←`/`→` still scroll.
- `--broker` can be given several times.

## 0.6.0 - 2026-10-28

### Added

- Publish dialog and panel: a payload that starts like JSON (`{` or `[`) is colored while you type it, in the editor and in the review: keys, strings, numbers and `true`/`false`/`null`. The cursor stays visible; invalid or half-typed JSON is colored as far as it goes, and lines longer than 4,096 characters stay plain.
- System topics per broker: `y` in the broker manager shows or hides the broker's `$SYS/#` topics (off by default; `mate broker edit <id> --system-topics true`). `#` never matches `$SYS`, so they need their own subscription. The option is saved in the profile and applies at once to a connected broker.
- The terminal title shows the broker and its connection state (`Plant A – Connected · MATE`), so the task bar tells several windows apart. Terminals that keep a title stack get their old title back on exit.
- `mate --connect` connects to the selected broker right away (`--broker`, else the default or only broker). A profile with `auto_connect = true` (`mate broker edit <id> --auto-connect true`, or `broker add --auto-connect`) connects when it is the broker MATE starts with. The broker manager shows the setting.

### Fixed

- Several MATE instances sharing one configuration no longer overwrite each other's saved layout, favorites and publish history in `ui-state.toml`: a save keeps what another instance changed and only replaces what this instance changed itself.

## 0.5.0 - 2026-10-26

### Added

- Exclusions: a profile can list topic filters whose messages are dropped although a subscription delivers them, for example subscribe to `#` and exclude `+/hmi/#`. Add them with `x` in the subscription manager (they appear in the list as `exclude`, can be switched off with `Space` and removed with `d`) or with `mate broker exclude <id> <filter>` / `unexclude`. They apply to the running connection at once and are stored in `broker.toml`. This happens in MATE: the broker still sends the messages, and topics that were already received stay in the list until you connect to another broker.

### Changed

- Stream view: `+` expands and `-` collapses the selected entry, matching `Enter`, which toggles it. `-` no longer collapses every entry at once.
- Quitting with `q` or `Ctrl+Q` now asks for confirmation (`y` or `Enter` quits, `n` or `Esc` stays; `Ctrl+Q` twice also quits). `Esc` never quit the program.
- The path field of the certificate import and reference dialog has a free cursor: `←`/`→`, `Home`/`End`, `Delete`, insertion and pasting at the cursor.

## 0.4.0 - 2026-10-26

### Added

- Stream view (`S`): every received message and connection event in arrival order (last 5,000 entries, 32 MiB of payload), newest last and following. `Enter` expands a message to QoS, retain flag, size, time since the previous message on the topic, MQTT 5 properties and the formatted payload; `Space` pauses (new entries wait), `/` filters by topic, `f` finds text in payloads and properties, `R` shows retained messages only, `e` toggles connection events, `s` shows the topic in the explorer, `c`/`t` copy payload or topic.
- Publish panel (`P`): the publish editor docked below the explorer panes, so topics and inspector stay visible while writing. It takes typing while focused (`Esc` leaves it), keeps its draft when hidden or after sending, and shows the review as a pop-up. The dialog (`p`) stays and `p` focuses the panel while it is shown.
- History search: `/` in the history panel filters the messages of the selected topic by payload text and MQTT 5 properties (`origin=plc-7`, content type, response topic, correlation data); the filter shows in the panel title and `Esc` clears it.
- Analytics: `/` filters every list by topic name, `Space` pauses the list so it can be read (rows and ages freeze).
- Publish dialog: `Ctrl+O` loads the payload from a UTF-8 text file.
- Analytics view (`A`) with seven lists, switched with `Tab` or `1`-`7`: topics by message frequency (count or rate, ascending or descending, with interval and time since the last message), topics whose payload is not valid JSON (broken JSON, text, binary, empty; filterable), quiet topics (silence and how overdue a topic is compared with its usual interval), jitter of the message timing, largest payloads (latest and peak), retained topics and per-branch totals with a selectable depth. `s` changes the sort column, `r` the order, `Enter` shows the topic in the explorer, `t` copies it. The lists are snapshots refreshed about once per second and show at most 2,000 rows.
- The inspector shows the MQTT 5 properties of the displayed message in one line under the message data (`MQTT 5  origin=plc-7  ·  site=hall 2  ·  type application/json`), user properties first; messages without properties show no such line.
- Broker and certificate editors: `↑`/`↓` move between the fields like `Tab`/`Shift+Tab` (the subscription field is still skipped when editing an existing profile).

### Changed

- The help is organised in tabs (Keys, Views, Concepts, Changelog, About) with scrolling instead of one long list. It opens on the section that matches the current view and `?` now also works in the analytics view and the broker manager. The changelog tab shows the CHANGELOG of the running build.
- `a` no longer opens the broker editor from the topic explorer; add brokers in the broker manager (`b`, then `a`). The shortcut bar offers `A` Analytics.
- The metadata view (`i`) lists the MQTT 5 properties right after the message data instead of at the bottom, so user properties are visible without scrolling on small terminals.

## 0.3.0 - 2026-10-26

### Added

- **Windows build:** CI cross-compiles `mate.exe` (x86_64, GNU target) on every pipeline, and tagged releases publish `mate-<version>-x86_64-windows.zip` with a checksum next to the Linux archive. The executable depends on system DLLs only, starts under Wine and was confirmed to run in a Windows VM (QEMU) with the pipeline artifact; it is unsigned and has not been tested in detail on Windows (credential store, certificate permissions). The 0.2.0 release got its Windows archive attached afterwards.
- **Message counts:** topic rows show `×N` behind the preview and the History title shows the count of the selected topic, both counted since the last connection attempt and abbreviated (`999`, `1.2k`, `15k`, `1.5M`). The count is kept per topic and is independent of the bounded history. It counts what MATE received; a broker may drop QoS 0 messages for a client that cannot keep up with a burst.
- **Send a history message again:** `Ctrl+S` in the history panel (or on a pinned message in the inspector) sends the selected message again exactly as received: topic, raw payload bytes, QoS and retain flag. It needs publishing to be allowed and an established connection, does nothing in the topic panel, and the result is reported and added to the publish history. Verified against Mosquitto.
- **Topic tree:** `+` / `-` in the topic panel expand the whole tree or collapse it to the top level; `Shift+Right` / `Shift+Left` open or close the selected branch together with everything below it. `Ctrl+Left/Right` stays reserved for resizing the panes.

### Changed

- The release text is generated from this changelog (the section of the tagged version); a tag without a matching section fails the release job.
- The release job packages the binaries built earlier in the pipeline instead of compiling again.
- CI caches are kept per job so parallel jobs no longer overwrite each other's build directory.

## 0.2.0 - 2026-10-23

### Added

- Publish dialog: `Ctrl+S` sends without the review step (mistakes such as deleting a retained message or invalid JSON still go to the review); the topic field has a free cursor too.
- Publish dialog: the payload field is a multi-line editor with a visible cursor (arrows, Home/End, Delete, line breaks with `Ctrl+J`/`Alt+Enter`), `Ctrl+F` formats JSON, `Ctrl+U` clears; the review screen shows the payload lines.
- Publish history: the publish dialog lists the last 20 messages sent to the broker below the fields; one can be selected (`↓`/`Tab`), loaded with `Enter`, edited and sent again through the normal review. Stored per broker in `ui-state.toml`; only successful publishes, identical messages once, payloads up to 16 KiB.
- `unique_client_id` profile setting (default on) and `broker edit --unique-client-id true|false`.

### Fixed

- Two MATE instances using the same profile no longer disconnect each other: every connection appends a random suffix to the client ID. Reproduced against Mosquitto (a shared fixed ID drops the older session, both reconnect) and verified fixed. Setting `--client-id` explicitly keeps that exact ID and turns the suffix off.

## 0.1.0 - 2026-10-23

First tagged release. Linux is the supported platform; Windows is only cross-compiled in CI.

### Added

- MIT license.
- Tagged builds (`vX.Y.Z`) publish a Linux x86_64 binary archive with a checksum and create a release; the source can be installed with `cargo install --git ... --tag vX.Y.Z --locked`.

- MQTT 5 connections (`rumqttc::v5`) next to MQTT 3.1.1, selected per profile (`protocol = "mqtt5"`, `--protocol`, Protocol field in the TUI editor). Verified against Mosquitto 2: connect, retained delivery, publish, 3.1.1 and 5 clients exchanging messages.
- Several subscriptions per broker, each with QoS and an `enabled` flag (older profiles load as enabled). TUI subscription manager (`s` in the broker manager: toggle, add, remove, QoS) and CLI `broker subscriptions|subscribe|unsubscribe|subscription-enable|subscription-disable`. Changes reach a running connection immediately (`BrokerRuntime::subscribe/unsubscribe`, verified for both protocol versions); disabled filters are not resubscribed after a reconnect.
- Clipboard: `t` copies the topic, `c` copies the payload in the topic and history panels, `Ctrl+C` copies (value in the inspector, otherwise the payload). Copying uses `wl-copy`, `xclip` or `xsel` and falls back to OSC 52; the notice names the method. Verified with `wl-copy` on Wayland.
- `Ctrl+Q` quits.
- Topic metadata view (`i`): message facts (time and age, size, QoS, retain, DUP, payload type, change vs. previous), per-topic statistics (messages received, history size, interval average/min/max, changed payloads), MQTT 5 properties (content type, payload format, expiry, response topic, correlation data, user properties, subscription IDs) and subtree totals for groups. Properties are read from the PUBLISH packet and verified against Mosquitto.
- User guide: step-by-step tutorial; new screenshots of the explorer, history layout and help overlay.

### Changed

- `Ctrl+C` no longer quits; it copies. Quit with `q` or `Ctrl+Q`.
- The broker editor edits the protocol and, for existing profiles, no longer overwrites the subscription list (managed in the subscription manager).
- Windows is not a current focus; it is still cross-compiled in CI.

### Known limitations

- MQTT 5 properties are shown in the metadata view only; they are not searchable and not part of the history list.
- The broker manager and broker editor screenshots predate the new keys.

## 0.0.3 - Terminal UI/UX redesign

### Added

- Two-pane default layout (topics 35 %, payload inspector 65 %), optional three-pane layout with message history (25/45/30 %), per-panel maximizing (`m`), adjustable pane widths (`<` `>`, `Ctrl+Left/Right`), and responsive fallbacks (history stacked below the inspector at medium widths, one pane at a time below 60 columns). Selection is kept by topic path across resizes.
- Topic explorer: compact one-line previews (`RUNNING`, `42.5`, `{…}`, `[12 items]`, `!` for invalid JSON) instead of inline JSON, truncation by display width, activity dots derived from message timestamps, incremental case-insensitive/fuzzy search with match highlighting, session favorites, page/jump keys, and viewport-only row rendering.
- Interactive JSON inspector: collapsible tree with JSON paths, search, horizontal scrolling, raw/pretty view with syntax colors, text/hex views for non-JSON payloads, copy of the selected value or the unmodified payload (OSC 52), and a cached parse that is only redone when the displayed message changes. Expansion state and cursor survive new messages on the same topic.
- Bounded per-topic message history (100 messages / 512 KiB per topic, 128 MiB / 200,000 messages overall) with timestamp, size, QoS, retain flag and change marker; stable historical selection that new messages do not overwrite; `LIVE` / `HISTORY – hh:mm:ss` indicator; line diff between two messages or against the previous one.
- Status bar showing one authoritative connection state per broker (connecting, connected, reconnecting, disconnected, error) with last error, and, only when established, the MQTT version, TLS/mTLS use, measured incoming message rate and discovered topic count. Dropped messages are counted and shown.
- Data origin labels: `LIVE`, `RETAINED`, `CACHED`.
- Context-sensitive shortcut bar and an updated help overlay.
- Broker manager as a table (name, host, TLS state, status) with a details pane and a connection test (`t`); TLS state is derived from the certificate files.
- Broker editor accepts a username and a password (stored in the OS keyring, never in TOML); `mate broker password <id>` CLI command; disconnect (`x`).
- `CHANGELOG.md`.
- Duplicate profiles (`u` in the broker manager, `mate broker duplicate`) and a default profile (`*`, `mate broker default`); the table marks the default.
- Favorites are saved per broker and the layout (history pane, pane widths) is restored on start, both in `ui-state.toml` next to `config.toml`; `F` shows only favorites. A broken or unwritable file is reported and never blocks the UI.
- CI coverage job (cargo-llvm-cov, Cobertura report for merge requests, coverage badge) and README badges.
- `docs/user-guide.md` and screenshots in `docs/images`.
- Publishing: `p` opens a dialog (topic, payload, QoS, retain) with a review step and warnings for retained messages and for deleting a retained message. It is off unless the profile allows it (`w` in the broker manager, `mate broker edit <id> --allow-publish true`); the status bar shows `read-only` or `publish on`, and the MQTT runtime itself refuses to publish for read-only profiles. Pasted text (bracketed paste) goes into the focused field, so multi-line JSON can be pasted into the payload.
- `mate broker edit` accepts `--client-id` and `--tls true|false`; authentication failures name the user and client ID that were refused.
- Library modules without UI dependencies: `json_tree`, `preview`, `diff`, `fuzzy`, `stats`, `clipboard`.

### Changed

- Connection events are now explicit states (`ConnectionState`) and messages carry QoS, retain flag and timestamps; the previous status strings were removed.
- Connecting to another broker clears the previous broker's topics; reconnecting to the same broker keeps its cache and marks it `CACHED` until fresh messages arrive.
- Key changes: `h` toggles the history panel (collapse is `Left`), `o` connects (the inspector uses `c` for copy), `Enter` in the broker manager connects.
- The event loop redraws only when state changed (plus a 500 ms tick while connected or while data is present), caps drawing at about 30 frames per second, and keeps draining message bursts without sleeping.
- JSON payloads keep their original key order (`serde_json` `preserve_order`).
- Crate version is 0.0.3.

### Fixed

- The header could show "Connected" with "No broker selected"; the connected broker is now part of the connection state.
- Stale connection events and data from a previous broker can no longer leak into the next connection.

### Known limitations

- MQTT 5 is not implemented. Maximizing and panel focus are not persisted.
- OSC 52 copying depends on terminal support and was not verified in a real terminal.
- Windows is cross-compiled in CI but the TUI has not been run there.
