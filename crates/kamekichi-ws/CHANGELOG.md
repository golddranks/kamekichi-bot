# Changelog

## [Unreleased]

### Added

- `connect_with_headers` / `try_connect_with_headers` for sending extra
  HTTP headers (e.g. `Authorization`) during the opening handshake.
- Subprotocol negotiation via `subprotocols()` builder and `subprotocol()`
  getter (`Sec-WebSocket-Protocol`, RFC 6455 §4).
- `inner_mut()` for mutable access to the underlying stream.
- `send_ping(deadline)` for liveness detection.  If no frame arrives
  by the deadline, the next idle `read_message` returns
  `ConnectionError::PingTimeout`.
- `last_activity()` returns when the last frame was received.

### Changed

- Frame masking uses aligned u128-wide XOR (~25x faster for large payloads).
- **Breaking:** Flood detection and frame budget now apply to all frame
  types, not just control frames.  This closes a denial-of-service vector
  where a peer could send many small continuation frames to spin the read
  loop without triggering any limit.
- **Breaking:** Renamed `control_frame_budget()` → `frame_budget()`,
  `max_control_flood_score()` → `max_flood_score()`,
  `ConnectionError::ControlFlood` → `ConnectionError::Flood`.
- Flood relief on completed messages is now proportional to message size,
  so legitimate large-message traffic does not accumulate flood pressure.
- **Breaking:** `read_message` now returns `ReadStatus` instead of
  `Option<Message>`.  Use `last_activity()` to distinguish budget
  exhaustion (recent — connection alive) from I/O silence (stale —
  consider a ping).
- **Breaking:** Renamed `SendResult` → `SendStatus`.

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
