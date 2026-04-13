# Changelog

## [Unreleased]

### Added

- `connect_with_headers` / `try_connect_with_headers` for sending extra
  HTTP headers (e.g. `Authorization`) during the opening handshake.
- Subprotocol negotiation via `subprotocols()` builder and `subprotocol()`
  getter (`Sec-WebSocket-Protocol`, RFC 6455 §4).
- `inner_mut()` for mutable access to the underlying stream.

## [0.1.1]

### Added

- `#[non_exhaustive]` on `ConnectionError` and `CallerError`.
- `rust-version` (MSRV: 1.85) in Cargo.toml.
- Static `Send + Sync` assertions for error types.

### Changed

- Renamed feature `rand_core` to `rand`.
- Simplified `Rng::fill_bytes` signature (no longer returns `Result`).
- Re-exported `Error`, `ConnectionError`, `CallerError`, `Rng` from crate root.

## [0.1.0] - 2025-01-01

Initial release.

- RFC 6455 compliant WebSocket client.
- Zero-copy message reads.
- Reusable buffers across reconnects (`disconnect` / `try_connect`).
- Automatic ping/pong handling.
- Control frame flood detection.
- Configurable payload limits and buffer sizes.
