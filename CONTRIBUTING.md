# Contributing

Thank you for helping with MATE. Bug reports, ideas and pull requests are welcome.

## Before you start

- Open an issue for larger changes so the approach can be agreed on first.
- Read [AGENTS.md](AGENTS.md): it describes the module boundaries (domain code must not depend on Ratatui, MQTT I/O stays in `mqtt.rs`, drawing stays in `ui/render.rs`) and the conventions.
- By contributing you agree that your contribution is released under the [MIT license](LICENSE).

## Workflow

1. Fork the repository and work on a feature branch, not on `main`.
2. Keep a change focused. Add tests: UI behavior is tested without a terminal by driving `UiState` with `handle_key` and asserting on `TestBackend` screens (see the tests in `src/ui.rs`).
3. Keyboard shortcuts live in three places that must change together: the handler, `shortcut_items`/`render_help` in `src/ui/render.rs`, and the README key table.
4. Update [CHANGELOG.md](CHANGELOG.md) under `Unreleased` and [ROADMAP.md](ROADMAP.md) only with behavior that works end to end.
5. Raise the development version in `Cargo.toml` (`0.x.0-dev.N`) with every change that is merged.
6. Open a pull request against `main`. The CI must be green.

## Checks to run locally

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny check        # licenses and advisories, needs cargo-deny
```

The broker integration tests need a Mosquitto and are skipped unless `MATE_TEST_BROKER_HOST` is set (see the header of `tests/broker_integration.rs`). `demo/demo.sh up` starts two demo brokers with invented data if you want to try a change by hand.

## Dependencies

New dependencies need a license from the list in [deny.toml](deny.toml) and should be small and maintained. Release archives include a generated `THIRD-PARTY-LICENSES.txt` (`cargo about`, configured in `about.toml` and `about.hbs`).
