#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! A lightweight, low-level WebSocket (RFC 6455) client library.
//!
//! Works with any stream implementing [`std::io::Read`] + [`std::io::Write`]
//! — blocking, non-blocking, TCP, or TLS. No async runtime needed.
//!
//! # Connection lifecycle
//!
//! 1. **Create** — [`WebSocket::new`] allocates buffers (no I/O).
//!    Chain [`max_payload`](WebSocket::max_payload) /
//!    [`max_buf_size`](WebSocket::max_buf_size) to configure limits.
//! 2. **Connect** — [`connect`](WebSocket::connect) (or
//!    [`try_connect`](WebSocket::try_connect)) performs the HTTP upgrade
//!    handshake.  Pass extra headers with
//!    [`connect_with_headers`](WebSocket::connect_with_headers).
//! 3. **Message loop** — [`read_message`](WebSocket::read_message)
//!    returns [`ReadStatus`]: either a [`Message`] or
//!    [`Idle`](ReadStatus::Idle).  Check
//!    [`last_activity`](WebSocket::last_activity) for liveness.
//!    Send with [`send_text`](WebSocket::send_text) /
//!    [`send_binary`](WebSocket::send_binary).
//! 4. **Close** — [`send_close`](WebSocket::send_close), then keep
//!    calling [`read_message`](WebSocket::read_message) until
//!    [`Message::Close`] arrives.
//! 5. **Reuse** — [`disconnect`](WebSocket::disconnect) returns the
//!    streamless `WebSocket` and the stream separately; the `WebSocket`
//!    can be reconnected without reallocating.

mod error;
mod proto;
mod read_buf;
mod rng;
mod send_buf;

#[cfg(test)]
mod tests;

use std::io::{self, Read, Write};
use std::time::Instant;

pub use error::{CallerError, ConnectionError, Error};
use proto::*;
pub use rng::Rng;

/// Default maximum payload size: 16 MiB.
pub(crate) const DEFAULT_MAX_PAYLOAD: usize = 16 * 1024 * 1024;
/// Default target buffer capacity: 1 MiB.
/// This can be momentarily exceeded while reading a large payload,
/// but will be reclaimed later.
pub(crate) const DEFAULT_MAX_BUF_SIZE: usize = 1024 * 1024;

/// Outcome of a send operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStatus {
    /// Frame fully written to the stream.
    Done,
    /// Frame is queued in the send buffer but not yet (fully) flushed.
    /// Call [`WebSocket::flush`] to finish sending.
    Queued,
    /// A previously queued frame is still draining and the new frame was
    /// **not** built.  Call [`WebSocket::flush`] to finish the pending
    /// write, then retry the send.
    RetryLater,
}

/// A complete WebSocket message, borrowing from the WebSocket's
/// internal buffers.
#[derive(Debug)]
pub enum Message<'a> {
    /// A UTF-8 text message.
    Text(&'a str),
    /// A binary message.
    Binary(&'a [u8]),
    /// A close frame with optional status code and reason.
    Close(Option<u16>, &'a str),
}

/// Result of [`WebSocket::read_message`].
#[derive(Debug)]
pub enum ReadStatus<'a> {
    /// A complete message was read.
    Message(Message<'a>),
    /// No complete message is available.
    ///
    /// This covers non-blocking `WouldBlock`, read timeouts, and
    /// frame budget exhaustion.  Use
    /// [`last_activity`](WebSocket::last_activity) to tell whether
    /// the connection is alive: if recent, frames are flowing but no
    /// complete message was produced yet; if stale or `None`, the
    /// wire has been silent and a liveness probe (ping) may be warranted.
    Idle,
}

impl<'a> ReadStatus<'a> {
    /// Returns the contained message, or `None` if idle.
    pub fn message(self) -> Option<Message<'a>> {
        match self {
            ReadStatus::Message(m) => Some(m),
            ReadStatus::Idle => None,
        }
    }
}

/// A WebSocket client over an arbitrary byte stream.
pub struct WebSocket<S, R> {
    pub(crate) stream: S,
    pub(crate) bufs: Buffers,
    pub(crate) sess: Session,
    pub(crate) rng: R,
}

impl<S: Read + Write, R: Rng> WebSocket<S, R> {
    /// Read the next complete WebSocket message from the stream.
    ///
    /// Frames are read and reassembled per RFC 6455 §5–6.  The returned
    /// [`Message`] borrows from the WebSocket's internal read buffer —
    /// no copies or allocations for single-frame messages.
    ///
    /// Control frames (ping, pong, close) are handled inline: pings are
    /// answered with pongs automatically, and close frames echo the status
    /// code back before returning [`Message::Close`].
    ///
    /// Returns [`ReadStatus::Idle`] when no complete message is available.
    /// Check [`last_activity`](Self::last_activity) to distinguish
    /// budget exhaustion (recent activity — connection is alive) from
    /// I/O silence (stale or `None` — consider a
    /// [`send_ping`](Self::send_ping)).
    pub fn read_message(&mut self) -> Result<ReadStatus<'_>, Error> {
        if self.sess.close_state == CloseState::Closed {
            return Err(CallerError::Closing.into());
        }
        match self
            .bufs
            .read_message(&mut self.stream, &mut self.sess, &mut self.rng)
        {
            Ok(Some(msg)) => {
                let now = Instant::now();
                self.sess.last_activity = Some(now);
                self.sess.ping_deadline = None;
                Ok(ReadStatus::Message(msg))
            }
            Ok(None) => {
                // Budget exhausted — frames were processed.
                let now = Instant::now();
                self.sess.last_activity = Some(now);
                self.sess.ping_deadline = None;
                Ok(ReadStatus::Idle)
            }
            Err(e) if e.is_would_block() => {
                if let Some(deadline) = self.sess.ping_deadline
                    && Instant::now() >= deadline
                {
                    self.sess.ping_deadline = None;
                    self.sess.close_state = CloseState::Closed;
                    return Err(ConnectionError::PingTimeout.into());
                }
                Ok(ReadStatus::Idle)
            }
            Err(e) => {
                self.sess.close_state = CloseState::Closed;
                Err(e.into())
            }
        }
    }

    /// Build a frame and flush it to the stream.
    ///
    /// If a previous frame is still partially queued, it is flushed first.
    /// Returns `Ok(RetryLater)` if the previous frame could not be drained
    /// (the new frame was **not** built — flush and retry).  Returns
    /// `Ok(Queued)` if the new frame was built but only partially flushed
    /// — call [`flush`](Self::flush) to finish.
    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<SendStatus, Error> {
        if self.sess.close_state != CloseState::Open {
            return Err(CallerError::Closing.into());
        }
        if self.bufs.send.has_pending() {
            match self.bufs.send.flush(&mut self.stream) {
                Ok(()) => {}
                Err(e) if e.is_would_block() => return Ok(SendStatus::RetryLater),
                Err(e) => return Err(e.into()),
            }
        }
        self.bufs.send.clear();
        build_frame(&mut self.bufs.send, &mut self.rng, opcode, payload);
        match self.bufs.send.flush(&mut self.stream) {
            Ok(()) => {
                let _ = self.stream.flush();
                Ok(SendStatus::Done)
            }
            Err(e) if e.is_would_block() => Ok(SendStatus::Queued),
            Err(e) => Err(e.into()),
        }
    }

    /// Flush any queued send data to the stream.
    ///
    /// Call this after a send method returns [`SendStatus::Queued`] or
    /// [`SendStatus::RetryLater`] to finish writing.
    pub fn flush(&mut self) -> Result<SendStatus, Error> {
        if self.bufs.send.has_pending() {
            match self.bufs.send.flush(&mut self.stream) {
                Ok(()) => {
                    self.bufs
                        .send
                        .maybe_shrink(self.sess.max_buf_size, &mut self.rng);
                }
                Err(e) if e.is_would_block() => return Ok(SendStatus::Queued),
                Err(e) => return Err(e.into()),
            }
        }
        match self.stream.flush() {
            Ok(()) => Ok(SendStatus::Done),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(SendStatus::Queued)
            }
            Err(e) => Err(ConnectionError::Io(e).into()),
        }
    }

    /// Send a UTF-8 text frame.
    pub fn send_text(&mut self, text: &str) -> Result<SendStatus, Error> {
        let r = self.send_frame(OP_TEXT, text.as_bytes())?;
        if r == SendStatus::Done {
            self.bufs
                .send
                .maybe_shrink(self.sess.max_buf_size, &mut self.rng);
        }
        Ok(r)
    }

    /// Send a binary frame.
    pub fn send_binary(&mut self, data: &[u8]) -> Result<SendStatus, Error> {
        let r = self.send_frame(OP_BINARY, data)?;
        if r == SendStatus::Done {
            self.bufs
                .send
                .maybe_shrink(self.sess.max_buf_size, &mut self.rng);
        }
        Ok(r)
    }

    /// Send a ping frame for liveness detection.
    ///
    /// If no frame (pong or otherwise) is received by `deadline`,
    /// the next [`read_message`](Self::read_message) call that
    /// observes silence will return
    /// [`ConnectionError::PingTimeout`](crate::ConnectionError::PingTimeout).
    /// Any received frame clears the deadline — the connection is
    /// proven alive regardless of whether the frame is a pong.
    ///
    /// Only one deadline is active at a time; a second `send_ping`
    /// replaces the previous deadline.
    pub fn send_ping(&mut self, deadline: Instant) -> Result<SendStatus, Error> {
        let r = self.send_frame(OP_PING, &[])?;
        if r != SendStatus::RetryLater {
            self.sess.ping_deadline = Some(deadline);
        }
        Ok(r)
    }

    /// Send a close frame with the given status code and reason.
    ///
    /// The reason must be at most 123 bytes (RFC 6455 §5.5: control frame
    /// payloads are limited to 125 bytes, minus 2 for the status code).
    ///
    /// The connection enters the `CloseSent` state even if the frame is
    /// only [`Queued`](SendStatus::Queued) — the close is committed and
    /// no further data frames may be sent.  Call [`read_message`](Self::read_message)
    /// to await the server's close response.
    pub fn send_close(&mut self, code: u16, reason: &str) -> Result<SendStatus, Error> {
        validate_send_close_code(code)?;
        let reason = reason.as_bytes();
        if reason.len() > 123 {
            return Err(CallerError::CloseReasonTooLong(reason.len()).into());
        }
        let len = 2 + reason.len();
        let mut payload = [0u8; 125];
        payload[..2].copy_from_slice(&code.to_be_bytes());
        payload[2..len].copy_from_slice(reason);
        let r = self.send_frame(OP_CLOSE, &payload[..len])?;
        if r != SendStatus::RetryLater {
            self.sess.close_state = CloseState::CloseSent;
        }
        if r == SendStatus::Done {
            self.bufs
                .send
                .maybe_shrink(self.sess.max_buf_size, &mut self.rng);
        }
        Ok(r)
    }
}

impl<R: Rng> WebSocket<(), R> {
    /// Create a new WebSocket with internal buffers but no stream.
    ///
    /// Use [`connect`](Self::connect) to perform the opening handshake, or
    /// [`with_stream`](Self::with_stream) to wrap a pre-upgraded stream.
    ///
    /// The provided `rng` generates frame masking keys and drives
    /// probabilistic buffer shrinking.
    ///
    /// RFC 6455 §10.3 requires (MUST) that masking keys be
    /// unpredictable — this defends against a cache-poisoning attack
    /// where a broken HTTP proxy inspects bytes inside a CONNECT
    /// tunnel and an attacker-controlled payload is crafted to look
    /// like a cacheable HTTP response after XOR with a predicted mask.
    ///
    /// **Default recommendation: `ChaCha8Rng`** (from `rand_chacha`).
    /// Fast, cryptographically strong, and satisfies the RFC
    /// unconditionally.
    ///
    /// A non-cryptographic PRNG such as `Pcg64Mcg` is acceptable when
    /// **either** of the following holds:
    /// - the stream is TLS or another strongly encrypted transport
    ///   (the proxy cannot observe the masked bytes), or
    /// - all sent content is fully controlled by the developer or the
    ///   direct user of the code (no attacker-chosen payload reaches
    ///   the wire).
    pub fn new(rng: R) -> Self {
        WebSocket {
            stream: (),
            bufs: Buffers::new(),
            sess: Session::new(),
            rng,
        }
    }

    /// Wrap a pre-upgraded stream for framed messaging without handshaking.
    ///
    /// Use this when the WebSocket upgrade was handled externally (e.g. by
    /// a proxy) or in tests.
    pub fn with_stream<S>(self, stream: S) -> WebSocket<S, R> {
        self.replace_stream(stream).1
    }

    /// Like [`connect`](Self::connect), but returns the `WebSocket` and
    /// `stream` on failure so the caller can retry without reallocating.
    ///
    /// On a [`CallerError`] (e.g. invalid host/path), the stream is
    /// returned untouched.  On a [`ConnectionError`], the stream may
    /// have been partially used (HTTP request written) but the
    /// `WebSocket` is reset to a clean state ready for another
    /// [`connect`](Self::connect) or
    /// [`try_connect`](Self::try_connect) call.
    #[allow(clippy::result_large_err)]
    pub fn try_connect<S: Read + Write>(
        self,
        stream: S,
        host: &str,
        path: &str,
    ) -> Result<WebSocket<S, R>, (Error, Self, S)> {
        self.try_connect_with_headers(stream, host, path, &[])
    }

    /// Like [`try_connect`](Self::try_connect), but sends additional
    /// HTTP headers during the opening handshake.
    ///
    /// Each `(name, value)` pair is written as a header line after the
    /// standard WebSocket headers.  Both name and value are validated
    /// to reject CR/LF (preventing header injection).
    ///
    /// ```rust,ignore
    /// let ws = WebSocket::new(rng)
    ///     .try_connect_with_headers(stream, "example.com", "/ws", &[
    ///         ("Authorization", "Bearer tok_xxx"),
    ///         ("Origin", "https://example.com"),
    ///     ])?;
    /// ```
    #[allow(clippy::result_large_err)]
    pub fn try_connect_with_headers<S: Read + Write>(
        self,
        stream: S,
        host: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<WebSocket<S, R>, (Error, Self, S)> {
        if host.bytes().any(|b| b.is_ascii_control() || b == b' ')
            || path.bytes().any(|b| b.is_ascii_control() || b == b' ')
            || headers.iter().any(|&(n, v)| {
                n.bytes()
                    .any(|b| b.is_ascii_control() || b == b' ' || b == b':')
                    || v.bytes().any(|b| b.is_ascii_control())
            })
        {
            return Err((CallerError::InvalidHeaderValue.into(), self, stream));
        }
        let mut ws = self.with_stream(stream);
        if let Err(e) = ws.handshake(host, path, headers) {
            let (ws, stream) = ws.disconnect();
            return Err((e.into(), ws, stream));
        }
        Ok(ws)
    }

    /// Perform the WebSocket opening handshake over `stream`.
    ///
    /// Sends an HTTP/1.1 upgrade request to `GET {path}` on `{host}`, reads
    /// the server's response, and validates the `101 Switching Protocols`
    /// status and `Sec-WebSocket-Accept` header per RFC 6455 §4.
    ///
    /// `host` is sent as-is in the `Host` header — include a port when
    /// it differs from the default for the scheme (e.g. `"example.com:8443"`).
    ///
    /// To send additional headers (e.g. `Authorization`), use
    /// [`connect_with_headers`](Self::connect_with_headers).
    ///
    /// On success the returned WebSocket is ready for framed messaging via
    /// [`read_message`](WebSocket::read_message) and
    /// [`send_text`](WebSocket::send_text).
    ///
    /// On failure the `WebSocket` and `stream` are dropped.  Use
    /// [`try_connect`](Self::try_connect) to recover them for retry.
    pub fn connect<S: Read + Write>(
        self,
        stream: S,
        host: &str,
        path: &str,
    ) -> Result<WebSocket<S, R>, Error> {
        self.try_connect(stream, host, path).map_err(|(e, _, _)| e)
    }

    /// Like [`connect`](Self::connect), but sends additional HTTP
    /// headers during the opening handshake.
    ///
    /// On failure the `WebSocket` and `stream` are dropped.  Use
    /// [`try_connect_with_headers`](Self::try_connect_with_headers) to
    /// recover them for retry.
    pub fn connect_with_headers<S: Read + Write>(
        self,
        stream: S,
        host: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<WebSocket<S, R>, Error> {
        self.try_connect_with_headers(stream, host, path, headers)
            .map_err(|(e, _, _)| e)
    }
}

impl<S, R: Rng> WebSocket<S, R> {
    /// Returns a reference to the underlying stream.
    pub fn inner(&self) -> &S {
        &self.stream
    }

    /// Returns a mutable reference to the underlying stream.
    ///
    /// Useful for changing timeouts, toggling non-blocking mode, or
    /// other transport-level configuration while connected.
    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// When the last frame was received, or `None` if no frames have
    /// been received since connecting.
    ///
    /// Updated once per [`read_message`](WebSocket::read_message) call
    /// that processed at least one frame (data, control, or continuation).
    /// Use this to implement liveness detection: if `last_activity` is
    /// stale, send a ping via [`send_ping`](WebSocket::send_ping).
    pub fn last_activity(&self) -> Option<Instant> {
        self.sess.last_activity
    }

    /// Disconnect and return the stream and a streamless `WebSocket<()>`
    /// whose buffers can be reused for a subsequent
    /// [`connect`](WebSocket::connect).
    ///
    /// Any queued send data is discarded.  Call [`flush`](WebSocket::flush)
    /// before disconnecting if a partially-sent frame must be delivered.
    pub fn disconnect(self) -> (WebSocket<(), R>, S) {
        let (stream, mut ws) = self.replace_stream(());
        ws.bufs.read.clear();
        ws.bufs.line_ends.clear();
        ws.bufs.send.clear();
        ws.bufs.fragment_buf.clear();
        ws.sess.fragment_opcode = 0;
        ws.sess.close_state = CloseState::Open;
        ws.sess.flood_score = 0;
        ws.sess.negotiated_subprotocol = None;
        ws.sess.ping_deadline = None;
        ws.sess.last_activity = None;
        (ws, stream)
    }

    /// Set the maximum payload size accepted for a single frame or
    /// reassembled fragmented message.  Default: 16 MiB.
    /// Clamped to 125..=[`isize::MAX`].
    pub fn max_payload(mut self, max: usize) -> Self {
        self.sess.max_payload = max.clamp(125, isize::MAX as usize);
        self
    }

    /// Set the target buffer capacity.  After processing a large message
    /// the read buffer is shrunk back to this size.  Default: 1 MiB.
    /// Clamped to a minimum of 125.
    pub fn max_buf_size(mut self, max: usize) -> Self {
        self.sess.max_buf_size = max.max(125);
        self
    }

    /// Set the maximum number of frames that
    /// [`read_message`](WebSocket::read_message) will process without
    /// producing a message before returning [`ReadStatus::Idle`].
    /// Default: `usize::MAX` (unlimited).  Clamped to a minimum of 1.
    ///
    /// This gives the caller periodic control even when the server
    /// sends many control frames or continuation fragments without
    /// completing a message.
    pub fn frame_budget(mut self, budget: usize) -> Self {
        self.sess.frame_budget = budget.max(1);
        self
    }

    /// Set the threshold for frame flood detection.
    ///
    /// The score increments for each frame processed, decrements
    /// for each small read (≤ 127 bytes) from the OS, and
    /// decrements on each completed message proportionally to its
    /// size.  A burst of pre-buffered frames drives the score up;
    /// frames arriving one at a time keep it near zero.
    /// Default: 1000.
    pub fn max_flood_score(mut self, max: usize) -> Self {
        self.sess.max_flood_score = max;
        self
    }

    /// Request subprotocol negotiation during the handshake.
    ///
    /// The protocols are sent in the `Sec-WebSocket-Protocol` header as
    /// a comma-separated list, in preference order.  After a successful
    /// [`connect`](WebSocket::connect), call
    /// [`subprotocol`](WebSocket::subprotocol) to see which one the
    /// server selected (if any).
    pub fn subprotocols(mut self, protocols: &[&str]) -> Self {
        self.sess.subprotocols = if protocols.is_empty() {
            None
        } else {
            Some(protocols.join(", "))
        };
        self
    }

    /// Returns the subprotocol selected by the server, or `None` if no
    /// subprotocol negotiation took place.
    pub fn subprotocol(&self) -> Option<&str> {
        self.sess.negotiated_subprotocol.as_deref()
    }

    /// Move all buffers into a new `WebSocket<T>`, replacing the stream.
    fn replace_stream<T>(self, new_stream: T) -> (S, WebSocket<T, R>) {
        (
            self.stream,
            WebSocket {
                stream: new_stream,
                bufs: self.bufs,
                sess: self.sess,
                rng: self.rng,
            },
        )
    }
}
