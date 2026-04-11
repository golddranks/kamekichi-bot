# Changelog

## [0.1.1]

### Added
- `#![warn(missing_docs)]` — all public items are now documented.
- `#[non_exhaustive]` on `ConnectionError` and `CallerError`.
- `rust-version` (MSRV: 1.85) in Cargo.toml.
- `categories` and `keywords` in Cargo.toml.
- CI: build matrix with MSRV and stable.
- Static `Send + Sync` assertions for error types.
- CHANGELOG.md, README.md.

### Changed
- Renamed feature `rand_core` to `rand`.
- Simplified `Rng::fill_bytes` signature (no longer returns `Result`).
- Fixed license field to valid SPDX (`MIT OR Apache-2.0`).
- Extracted error types into `error` module.
- Extracted `Rng` trait into `rng` module.
- Re-exported `Error`, `ConnectionError`, `CallerError`, `Rng` from crate root.

## [0.1.0] - 2025-01-01

Initial release.

- RFC 6455 compliant WebSocket client.
- Zero-copy message reads.
- Reusable buffers across reconnects (`disconnect` / `try_connect`).
- Automatic ping/pong handling.
- Control frame flood detection.
- Configurable payload limits and buffer sizes.
