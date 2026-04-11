# kamekichi-ws

A lightweight, robust, low-level WebSocket (RFC 6455) client library for Rust.

- Bring your own stream — works with blocking, non-blocking, TCP and TLS streams alike. No async runtime needed.
- Zero-copy message reads — `Message` borrows directly from internal buffers.
- Allocation-conscious — buffer allocations are front-loaded and reusable across reconnects and shrink back after large messages.
- 100% test coverage.
- No `unsafe`.
- Minimal dependencies: `base64`, `ring`, and optionally `rand_core`.

## Usage

```rust
use kamekichi_ws::{WebSocket, Message};

let ws = WebSocket::new(rng);
let mut ws = ws.connect(stream, "example.com", "/?v=10&encoding=json")?;

ws.send_text(r#"{"op":1}"#)?;

loop {
    match ws.read_message()? {
        Some(Message::Text(t)) => println!("{t}"),
        Some(Message::Binary(b)) => println!("{} bytes", b.len()),
        Some(Message::Close(code, reason)) => break,
        None => { /* WouldBlock / TimeOut / frame budget exhausted */ }
    }
}
```

## RNG

`WebSocket` is generic over an `Rng` trait for frame masking. Enable the `rand` feature to use any `rand_core::Rng` directly:

```toml
kamekichi-ws = { version = "0.1", features = ["rand"] }
```

## License

MIT / Apache-2.0
