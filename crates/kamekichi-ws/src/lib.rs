#![forbid(unsafe_code)]

mod read_buf;
mod send_buf;

#[cfg(test)]
mod tests;

use std::{
    io::{self, Read, Write},
    str::Utf8Error,
};

use read_buf::{FillError, ReadBuf};
use send_buf::SendBuf;

use base64::Engine;
use rand_core::Rng;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
pub const DEFAULT_MAX_PAYLOAD: usize = 16 * 1024 * 1024; // 16 MiB
pub const DEFAULT_MAX_BUF_SIZE: usize = 1024 * 1024; // 1 MiB
const MAX_HEADER: usize = 8192;
/// Maximum wire size of a single control frame (2-byte header + 125-byte
/// payload).  Reads of this size or smaller delivered at most one control
/// frame and count as "trickle" for flood detection.
const SMALL_READ_THRESHOLD: usize = 127;

/// Reject values containing CR or LF to prevent CRLF header injection.
fn validate_header_value(value: &str) -> Result<(), CallerError> {
    if value.bytes().any(|b| b == b'\r' || b == b'\n') {
        return Err(CallerError::InvalidHeaderValue);
    }
    Ok(())
}

// Frame header bits (RFC 6455 §5.2)
const FIN: u8 = 0b1000_0000;
const RESERVED_MASK: u8 = 0b0111_0000;
const OPCODE_MASK: u8 = 0b0000_1111;
const MASK_BIT: u8 = 0b1000_0000;
const LEN_MASK: u8 = 0b0111_1111;

// Opcodes (RFC 6455 §5.2)
const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Outcome of a send operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendResult {
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
    Text(&'a str),
    Binary(&'a [u8]),
    Close(Option<u16>, &'a str),
}

/// Connection-level error.  The connection is dead or protocol-violated;
/// the caller should reconnect.
#[derive(Debug)]
pub enum ConnectionError {
    Io(io::Error),
    Closed,
    HeadersTooLarge,
    BadStatus,
    MissingAccept,
    BadAccept,
    BadReservedBits(u8),
    PayloadTooLarge(u64),
    UnexpectedContinuation,
    FragmentedMessageTooLarge(usize),
    InvalidUtf8(Utf8Error),
    DataDuringFragmentation,
    FragmentedControl,
    ControlPayloadTooLarge(usize),
    BadClosePayload,
    UnknownOpcode(u8),
    NonMinimalLength,
    PayloadLengthMsb,
    MissingUpgrade,
    MissingConnection,
    InvalidCloseCode(u16),
    MaskedServerFrame,
    ControlFlood,
}

impl ConnectionError {
    fn is_would_block(&self) -> bool {
        matches!(
            self,
            ConnectionError::Io(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
        )
    }
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionError::Io(e) => write!(f, "IO: {e}"),
            ConnectionError::Closed => write!(f, "connection closed"),
            ConnectionError::HeadersTooLarge => write!(f, "response headers too large"),
            ConnectionError::BadStatus => write!(f, "expected HTTP 101"),
            ConnectionError::MissingAccept => write!(f, "missing Sec-WebSocket-Accept"),
            ConnectionError::BadAccept => write!(f, "invalid Sec-WebSocket-Accept"),
            ConnectionError::BadReservedBits(bits) => write!(f, "non-zero RSV bits: {bits:#x}"),
            ConnectionError::PayloadTooLarge(len) => {
                write!(f, "frame payload too large: {len} bytes")
            }
            ConnectionError::UnexpectedContinuation => write!(f, "unexpected continuation frame"),
            ConnectionError::FragmentedMessageTooLarge(len) => {
                write!(f, "fragmented message too large: {len} bytes")
            }
            ConnectionError::InvalidUtf8(e) => {
                write!(f, "invalid UTF-8 in text frame at {}", e.valid_up_to())
            }
            ConnectionError::DataDuringFragmentation => {
                write!(f, "new data frame during fragmentation")
            }
            ConnectionError::FragmentedControl => write!(f, "fragmented control frame"),
            ConnectionError::ControlPayloadTooLarge(len) => {
                write!(f, "control frame payload too large: {len}")
            }
            ConnectionError::BadClosePayload => {
                write!(f, "close frame payload must be 0 or >= 2 bytes")
            }
            ConnectionError::UnknownOpcode(op) => write!(f, "unknown opcode: {op}"),
            ConnectionError::NonMinimalLength => write!(f, "non-minimal payload length encoding"),
            ConnectionError::PayloadLengthMsb => {
                write!(f, "64-bit payload length has MSB set (RFC 6455 §5.2)")
            }
            ConnectionError::MissingUpgrade => write!(f, "missing or invalid Upgrade header"),
            ConnectionError::MissingConnection => {
                write!(f, "missing or invalid Connection header")
            }
            ConnectionError::InvalidCloseCode(code) => write!(f, "invalid close code: {code}"),
            ConnectionError::MaskedServerFrame => write!(f, "server sent a masked frame"),
            ConnectionError::ControlFlood => write!(f, "control frame flood detected"),
        }
    }
}

impl std::error::Error for ConnectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConnectionError::Io(e) => Some(e),
            ConnectionError::InvalidUtf8(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ConnectionError {
    fn from(e: io::Error) -> Self {
        ConnectionError::Io(e)
    }
}

impl From<Utf8Error> for ConnectionError {
    fn from(e: Utf8Error) -> Self {
        ConnectionError::InvalidUtf8(e)
    }
}

impl From<FillError> for ConnectionError {
    fn from(e: FillError) -> Self {
        match e {
            FillError::Eof => ConnectionError::Closed,
            FillError::BufferFull => ConnectionError::HeadersTooLarge,
            FillError::Io(e) => e.into(),
        }
    }
}

/// Caller-side error.  Reconnecting will not help.
#[derive(Debug)]
pub enum CallerError {
    /// The connection is closing; no further operations are allowed.
    Closing,
    /// `host` or `path` passed to [`connect`](WebSocket::connect) contains CR or LF.
    InvalidHeaderValue,
    /// Close code is not valid for sending (RFC 6455 §7.4).
    InvalidCloseCode(u16),
    /// Close reason exceeds 123 bytes.
    CloseReasonTooLong(usize),
}

impl std::fmt::Display for CallerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallerError::Closing => write!(f, "connection is closing"),
            CallerError::InvalidHeaderValue => write!(f, "invalid character in header value"),
            CallerError::InvalidCloseCode(code) => write!(f, "invalid close code: {code}"),
            CallerError::CloseReasonTooLong(len) => {
                write!(f, "close reason too long: {len} bytes (max 123)")
            }
        }
    }
}

impl std::error::Error for CallerError {}

/// Actionable error from a WebSocket operation.
#[derive(Debug)]
pub enum Error {
    /// The connection is dead or protocol-violated.  Reconnect.
    Reconnect(ConnectionError),
    /// Caller error.  Reconnecting will not help.
    Fatal(CallerError),
}

impl From<ConnectionError> for Error {
    fn from(e: ConnectionError) -> Self {
        Error::Reconnect(e)
    }
}

impl From<CallerError> for Error {
    fn from(e: CallerError) -> Self {
        Error::Fatal(e)
    }
}

impl From<FillError> for Error {
    fn from(e: FillError) -> Self {
        Error::Reconnect(e.into())
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Reconnect(ConnectionError::Io(e))
    }
}

impl From<Utf8Error> for Error {
    fn from(e: Utf8Error) -> Self {
        Error::Reconnect(ConnectionError::InvalidUtf8(e))
    }
}

impl Error {
    fn is_would_block(&self) -> bool {
        matches!(self, Error::Reconnect(e) if e.is_would_block())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Reconnect(e) => e.fmt(f),
            Error::Fatal(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Reconnect(e) => Some(e),
            Error::Fatal(e) => Some(e),
        }
    }
}

/// Close handshake state (RFC 6455 §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseState {
    /// Connection is open — reads and writes allowed.
    Open,
    /// We sent a close frame.  Writes are blocked; reads continue
    /// until the server's close response arrives.
    CloseSent,
    /// Close handshake complete (both sides).  No further I/O.
    Closed,
}

/// I/O buffers.  Separated from [`Session`] so that `read_message` can
/// lend the returned [`Message`] from these buffers while [`Session`]
/// remains free to mutate (e.g. poisoning `close_state` on error).
struct Buffers {
    /// Incoming byte buffer.  Non-fragmented messages are borrowed
    /// directly from here, avoiding copies.
    read: ReadBuf,
    /// Scratch space for line-end positions during header parsing.
    line_ends: Vec<usize>,
    /// Outgoing byte buffer; partially-flushed frames are retried on
    /// the next flush.
    send: SendBuf,
    /// Reassembly buffer for fragmented messages.  Each continuation
    /// frame's payload is appended here; the completed message borrows
    /// from this buffer.
    fragment_buf: Vec<u8>,
}

/// Protocol state that is never borrowed by a returned [`Message`].
struct Session {
    /// Opcode of the first frame in a fragmented message (0 = no active fragmentation).
    fragment_opcode: u8,
    close_state: CloseState,
    /// Maximum payload size (bytes) accepted for a single frame or
    /// reassembled fragmented message.
    max_payload: usize,
    /// Target buffer capacity. After processing a large message the read
    /// buffer is shrunk back to this size to avoid permanent memory bloat.
    max_buf_size: usize,
    /// Max control frames processed per [`read_message`] call before
    /// returning `Ok(None)`.  `usize::MAX` = unlimited.
    control_frame_budget: usize,
    /// Running score for control frame flood detection.  Incremented per
    /// control frame, decremented per small read, reset on data message.
    control_flood_score: usize,
    /// Threshold above which [`control_flood_score`] triggers an error.
    max_control_flood_score: usize,
}

/// A WebSocket client over an arbitrary byte stream.
pub struct WebSocket<S, R> {
    stream: S,
    bufs: Buffers,
    sess: Session,
    rng: R,
}

impl Buffers {
    fn read_message<S: Read + Write, R: Rng>(
        &mut self,
        stream: &mut S,
        sess: &mut Session,
        rng: &mut R,
    ) -> Result<Option<Message<'_>>, Error> {
        // If a previous send (e.g. a pong reply) was only partially
        // written to the stream, finish sending it now.
        if self.send.has_pending() {
            match self.send.flush(stream) {
                Ok(()) => {
                    let _ = stream.flush();
                    self.send.maybe_shrink(sess.max_buf_size, rng);
                }
                // Don't propagate — this is a read call; returning
                // WouldBlock would make the caller wait for readability
                // when the real bottleneck is writability.  The bytes
                // stay in the send buffer and will go out on the next attempt
                // on a best-effort basis.
                Err(e) if e.is_would_block() => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut budget = sess.control_frame_budget;
        loop {
            if self.read.maybe_compact(MAX_HEADER) {
                self.read.maybe_shrink_capacity(sess.max_buf_size, rng);
            }
            // Shrink the fragment reassembly buffer after a large
            // fragmented message.  Done here (top of loop) rather than
            // at completion time because the returned Message borrows
            // from fragment_buf — we can only free the data once the
            // caller has dropped that borrow and re-entered read_message.
            if sess.fragment_opcode == 0
                && self.fragment_buf.capacity() > sess.max_buf_size
                && rng.next_u32() & 0b111 == 0
            {
                self.fragment_buf.clear();
                self.fragment_buf.shrink_to(sess.max_buf_size);
            }

            // Track how many bytes fill_from reads from the OS.
            let end_before = self.read.as_slice().len();

            // Parse frame header
            let (fin, opcode, payload_len) = {
                self.read.fill_from(stream, 2)?;
                let hdr = self.read.pending();
                let byte0 = hdr[0];
                let byte1 = hdr[1];

                let rsv = byte0 & RESERVED_MASK;
                if rsv != 0 {
                    return Err(ConnectionError::BadReservedBits(rsv).into());
                }

                let fin = byte0 & FIN != 0;
                let opcode = byte0 & OPCODE_MASK;
                let masked = byte1 & MASK_BIT != 0;
                if masked {
                    return Err(ConnectionError::MaskedServerFrame.into());
                }
                let len_byte = (byte1 & LEN_MASK) as usize;

                let (header_size, payload_len) = match len_byte {
                    n @ 0..=125 => (2, n),
                    126 => {
                        self.read.fill_from(stream, 4)?;
                        let hdr = self.read.pending();
                        let len = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
                        if len < 126 {
                            return Err(ConnectionError::NonMinimalLength.into());
                        }
                        (4, len)
                    }
                    127 => {
                        self.read.fill_from(stream, 10)?;
                        let hdr = self.read.pending();
                        let len = u64::from_be_bytes(hdr[2..10].try_into().unwrap());
                        if len >> 63 != 0 {
                            return Err(ConnectionError::PayloadLengthMsb.into());
                        }
                        if len < 65536 {
                            return Err(ConnectionError::NonMinimalLength.into());
                        }
                        if len > sess.max_payload as u64 {
                            return Err(ConnectionError::PayloadTooLarge(len).into());
                        }
                        (10, len as usize)
                    }
                    _ => unreachable!("7-bit value is always 0..=127"),
                };

                if opcode >= OP_CLOSE {
                    if !fin {
                        return Err(ConnectionError::FragmentedControl.into());
                    }
                    if payload_len > 125 {
                        return Err(ConnectionError::ControlPayloadTooLarge(payload_len).into());
                    }
                } else if payload_len > sess.max_payload {
                    return Err(ConnectionError::PayloadTooLarge(payload_len as u64).into());
                }

                let frame_size = header_size + payload_len;
                self.read.fill_from(stream, frame_size)?;
                self.read.consume(frame_size);

                (fin, opcode, payload_len)
            };

            // A small read means data trickled in (one ping); a big
            // read (or no read at all) means data was already buffered.
            let bytes_read = self.read.as_slice().len() - end_before;
            if bytes_read > 0 && bytes_read <= SMALL_READ_THRESHOLD {
                sess.control_flood_score = sess.control_flood_score.saturating_sub(1);
            }

            let pos = self.read.pos();
            let payload_range = pos - payload_len..pos;

            match opcode {
                OP_CONTINUATION => {
                    if sess.fragment_opcode == 0 {
                        return Err(ConnectionError::UnexpectedContinuation.into());
                    }
                    let total = self.fragment_buf.len() + payload_len;
                    if total > sess.max_payload {
                        return Err(ConnectionError::FragmentedMessageTooLarge(total).into());
                    }
                    self.fragment_buf
                        .extend_from_slice(&self.read.as_slice()[payload_range]);
                    if fin {
                        let opcode = sess.fragment_opcode;
                        sess.fragment_opcode = 0;
                        sess.control_flood_score = 0;
                        return into_message(opcode, &self.fragment_buf).map(Some);
                    }
                }

                OP_TEXT | OP_BINARY => {
                    if sess.fragment_opcode != 0 {
                        return Err(ConnectionError::DataDuringFragmentation.into());
                    }
                    if fin {
                        sess.control_flood_score = 0;
                        return into_message(opcode, &self.read.as_slice()[payload_range])
                            .map(Some);
                    }
                    self.fragment_buf.clear();
                    self.fragment_buf
                        .extend_from_slice(&self.read.as_slice()[payload_range]);
                    sess.fragment_opcode = opcode;
                }

                OP_CLOSE | OP_PING | OP_PONG =>
                {
                    #[allow(clippy::wildcard_in_or_patterns)]
                    match opcode {
                        OP_CLOSE => {
                            let echo = sess.close_state == CloseState::Open;
                            sess.close_state = CloseState::Closed;
                            return handle_close(
                                stream,
                                &mut self.send,
                                rng,
                                self.read.get(payload_range),
                                echo,
                            )
                            .map(Some);
                        }
                        OP_PING | OP_PONG | _ => {
                            sess.control_flood_score += 1;
                            if sess.control_flood_score > sess.max_control_flood_score {
                                return Err(ConnectionError::ControlFlood.into());
                            }
                            if opcode == OP_PING {
                                if self.send.pending_len() < sess.max_buf_size {
                                    build_frame(
                                        &mut self.send,
                                        rng,
                                        OP_PONG,
                                        self.read.get(payload_range),
                                    );
                                }
                                self.send.try_flush(stream);
                            }
                            budget = budget.saturating_sub(1);
                            if budget == 0 {
                                return Ok(None);
                            }
                        }
                    }
                }

                _ => return Err(ConnectionError::UnknownOpcode(opcode).into()),
            }
        }
    }
}

impl<S: Read + Write, R: Rng> WebSocket<S, R> {
    /// Run the WebSocket opening handshake on the underlying stream.
    fn handshake(&mut self, host: &str, path: &str) -> Result<(), Error> {
        self.bufs.read.clear();
        self.bufs.send.clear();
        self.bufs.line_ends.clear();
        self.bufs.fragment_buf.clear();
        self.sess.fragment_opcode = 0;
        self.sess.close_state = CloseState::Open;
        self.sess.control_flood_score = 0;

        let key = {
            let mut raw = [0u8; 16];
            self.rng.fill_bytes(&mut raw);
            let mut encoded = [0u8; 24];
            base64::engine::general_purpose::STANDARD
                .encode_slice(raw, &mut encoded)
                .expect("24-byte buffer fits 16 bytes of base64");
            encoded
        };

        {
            let key = std::str::from_utf8(&key).expect("base64 output is ASCII");
            let buf = self.bufs.send.as_scratch();
            use std::io::Write as _;
            write!(
                buf,
                "GET {path} HTTP/1.1\r\n\
                 Host: {host}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: {key}\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n"
            )
            .expect("write to Vec cannot fail");
            self.stream.write_all(buf)?;
            self.bufs.send.clear();
            self.stream.flush()?;
        }

        let header_end = {
            let mut scan = 0;
            let mut prev_nl: Option<usize> = None;
            let line_ends = &mut self.bufs.line_ends;
            self.bufs
                .read
                .read_until::<_, Error>(&mut self.stream, MAX_HEADER, |buf| {
                    let data = buf.as_slice();
                    let filled = data.len();
                    while let Some(off) = data[scan..filled].iter().position(|&b| b == b'\n') {
                        let nl = scan + off;
                        line_ends.push(nl);
                        if let Some(prev) = prev_nl {
                            let gap = nl - prev;
                            if gap == 1 || (gap == 2 && data[nl - 1] == b'\r') {
                                return Ok(Some(nl + 1));
                            }
                        }
                        prev_nl = Some(nl);
                        scan = nl + 1;
                    }
                    if filled >= MAX_HEADER {
                        return Err(ConnectionError::HeadersTooLarge.into());
                    }
                    Ok(None)
                })?
        };

        {
            let status = b"HTTP/1.1 101";
            let data = self.bufs.read.as_slice();
            if !(data[..header_end].starts_with(status)
                && matches!(data[status.len()], b' ' | b'\r' | b'\n'))
            {
                return Err(ConnectionError::BadStatus.into());
            }
        }

        {
            let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
            ctx.update(&key);
            ctx.update(WS_GUID.as_bytes());
            let mut expected = [0u8; 28];
            base64::engine::general_purpose::STANDARD
                .encode_slice(ctx.finish().as_ref(), &mut expected)
                .expect("28-byte buffer fits 20 bytes of base64");

            let mut accept = None;
            let mut has_upgrade = false;
            let mut has_connection = false;
            let mut line_start = 0;
            let data = self.bufs.read.as_slice();
            for &nl in &self.bufs.line_ends {
                let line = &data[line_start..nl];
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if let Some(colon) = line.iter().position(|&b| b == b':') {
                    let name = &line[..colon];
                    let value = line[colon + 1..].trim_ascii();
                    if name.eq_ignore_ascii_case(b"sec-websocket-accept") {
                        accept = accept.or(Some(value));
                    } else if name.eq_ignore_ascii_case(b"upgrade") {
                        has_upgrade |= value
                            .split(|&b| b == b',')
                            .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"websocket"));
                    } else if name.eq_ignore_ascii_case(b"connection") {
                        has_connection |= value
                            .split(|&b| b == b',')
                            .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"upgrade"));
                    }
                }
                line_start = nl + 1;
            }

            let accept = accept.ok_or(ConnectionError::MissingAccept)?;
            if accept != expected {
                return Err(ConnectionError::BadAccept.into());
            }
            if !has_upgrade {
                return Err(ConnectionError::MissingUpgrade.into());
            }
            if !has_connection {
                return Err(ConnectionError::MissingConnection.into());
            }
        }

        self.bufs.read.consume(header_end);
        self.bufs.read.compact();
        self.bufs.line_ends.clear();
        self.bufs.line_ends.shrink_to(32);

        Ok(())
    }

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
    /// Returns `Ok(None)` if the underlying stream is non-blocking and no
    /// complete frame is available yet, in case of a timeout, or when the
    /// [`control_frame_budget`](WebSocket::control_frame_budget) is
    /// exhausted.
    pub fn read_message(&mut self) -> Result<Option<Message<'_>>, Error> {
        if self.sess.close_state == CloseState::Closed {
            return Err(CallerError::Closing.into());
        }
        match self
            .bufs
            .read_message(&mut self.stream, &mut self.sess, &mut self.rng)
        {
            Ok(msg) => Ok(msg),
            Err(e) if e.is_would_block() => Ok(None),
            Err(e) => {
                self.sess.close_state = CloseState::Closed;
                Err(e)
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
    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<SendResult, Error> {
        if self.sess.close_state != CloseState::Open {
            return Err(CallerError::Closing.into());
        }
        if self.bufs.send.has_pending() {
            match self.bufs.send.flush(&mut self.stream) {
                Ok(()) => {}
                Err(e) if e.is_would_block() => return Ok(SendResult::RetryLater),
                Err(e) => return Err(e.into()),
            }
        }
        self.bufs.send.clear();
        build_frame(&mut self.bufs.send, &mut self.rng, opcode, payload);
        match self.bufs.send.flush(&mut self.stream) {
            Ok(()) => {
                let _ = self.stream.flush();
                Ok(SendResult::Done)
            }
            Err(e) if e.is_would_block() => Ok(SendResult::Queued),
            Err(e) => Err(e.into()),
        }
    }

    /// Flush any queued send data to the stream.
    ///
    /// Call this after a send method returns [`SendResult::Queued`] or
    /// [`SendResult::RetryLater`] to finish writing.
    pub fn flush(&mut self) -> Result<SendResult, Error> {
        if self.bufs.send.has_pending() {
            match self.bufs.send.flush(&mut self.stream) {
                Ok(()) => {
                    self.bufs
                        .send
                        .maybe_shrink(self.sess.max_buf_size, &mut self.rng);
                }
                Err(e) if e.is_would_block() => return Ok(SendResult::Queued),
                Err(e) => return Err(e.into()),
            }
        }
        match self.stream.flush() {
            Ok(()) => Ok(SendResult::Done),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(SendResult::Queued)
            }
            Err(e) => Err(ConnectionError::Io(e).into()),
        }
    }

    /// Send a UTF-8 text frame.
    pub fn send_text(&mut self, text: &str) -> Result<SendResult, Error> {
        let r = self.send_frame(OP_TEXT, text.as_bytes())?;
        if r == SendResult::Done {
            self.bufs
                .send
                .maybe_shrink(self.sess.max_buf_size, &mut self.rng);
        }
        Ok(r)
    }

    /// Send a binary frame.
    pub fn send_binary(&mut self, data: &[u8]) -> Result<SendResult, Error> {
        let r = self.send_frame(OP_BINARY, data)?;
        if r == SendResult::Done {
            self.bufs
                .send
                .maybe_shrink(self.sess.max_buf_size, &mut self.rng);
        }
        Ok(r)
    }

    /// Send a close frame with the given status code and reason.
    ///
    /// The reason must be at most 123 bytes (RFC 6455 §5.5: control frame
    /// payloads are limited to 125 bytes, minus 2 for the status code).
    ///
    /// The connection enters the `CloseSent` state even if the frame is
    /// only [`Queued`](SendResult::Queued) — the close is committed and
    /// no further data frames may be sent.  Call [`read_message`](Self::read_message)
    /// to await the server's close response.
    pub fn send_close(&mut self, code: u16, reason: &str) -> Result<SendResult, Error> {
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
        if r != SendResult::RetryLater {
            self.sess.close_state = CloseState::CloseSent;
        }
        if r == SendResult::Done {
            self.bufs
                .send
                .maybe_shrink(self.sess.max_buf_size, &mut self.rng);
        }
        Ok(r)
    }
}

/// Append a masked WebSocket frame to the send buffer.
fn build_frame(send: &mut SendBuf, rng: &mut impl Rng, opcode: u8, payload: &[u8]) {
    let mask = rng.next_u32().to_ne_bytes();
    send.reserve(2 + 8 + 4 + payload.len());
    send.push_byte(FIN | opcode);

    let len = payload.len();
    if len < 126 {
        send.push_byte(MASK_BIT | len as u8);
    } else if len < 65536 {
        send.push_byte(MASK_BIT | 126);
        send.push(&(len as u16).to_be_bytes());
    } else {
        send.push_byte(MASK_BIT | 127);
        send.push(&(len as u64).to_be_bytes());
    }
    send.push(&mask);
    send.push(payload);

    let buf = send.last_mut(payload.len());
    for (i, b) in buf.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
}

fn validate_send_close_code(code: u16) -> Result<(), CallerError> {
    match code {
        1000..=1003 | 1007..=1014 | 3000..=4999 => Ok(()),
        _ => Err(CallerError::InvalidCloseCode(code)),
    }
}

fn validate_recv_close_code(code: u16) -> Result<(), ConnectionError> {
    match code {
        1005 | 1006 | 1015 => Err(ConnectionError::InvalidCloseCode(code)),
        1000..=4999 => Ok(()),
        _ => Err(ConnectionError::InvalidCloseCode(code)),
    }
}

fn into_message<'a>(opcode: u8, buf: &'a [u8]) -> Result<Message<'a>, Error> {
    match opcode {
        OP_TEXT => Ok(Message::Text(std::str::from_utf8(buf)?)),
        OP_BINARY => Ok(Message::Binary(buf)),
        _ => unreachable!("into_message called with non-data opcode {opcode}"),
    }
}

/// Handle an incoming close frame: validate, echo if needed, return.
fn handle_close<'a>(
    stream: &mut (impl Read + Write),
    send: &mut SendBuf,
    rng: &mut impl Rng,
    payload: &'a [u8],
    echo: bool,
) -> Result<Message<'a>, Error> {
    if payload.len() == 1 {
        return Err(ConnectionError::BadClosePayload.into());
    }
    let (code, reason) = if payload.len() >= 2 {
        let code = u16::from_be_bytes([payload[0], payload[1]]);
        validate_recv_close_code(code)?;
        let reason = std::str::from_utf8(&payload[2..])?;
        (Some(code), reason)
    } else {
        (None, "")
    };
    if echo {
        // Best-effort: if the echo fails the caller still gets
        // Message::Close with the code and reason.  Propagating
        // the IO error would hide the close payload — worse,
        // since the server already decided to close and our
        // echo is just protocol politeness.
        build_frame(send, rng, OP_CLOSE, &payload[..payload.len().min(2)]);
        send.try_flush(stream);
    }
    Ok(Message::Close(code, reason))
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
            bufs: Buffers {
                read: ReadBuf::with_capacity(MAX_HEADER),
                line_ends: Vec::with_capacity(16),
                send: SendBuf::with_capacity(256),
                fragment_buf: Vec::new(),
            },
            sess: Session {
                fragment_opcode: 0,
                close_state: CloseState::Open,
                max_payload: DEFAULT_MAX_PAYLOAD,
                max_buf_size: DEFAULT_MAX_BUF_SIZE,
                control_frame_budget: usize::MAX,
                control_flood_score: 0,
                max_control_flood_score: 1000,
            },
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
        if let Err(e) = validate_header_value(host) {
            return Err((e.into(), self, stream));
        }
        if let Err(e) = validate_header_value(path) {
            return Err((e.into(), self, stream));
        }
        let mut ws = self.with_stream(stream);
        if let Err(e) = ws.handshake(host, path) {
            let (ws, stream) = ws.disconnect();
            return Err((e, ws, stream));
        }
        Ok(ws)
    }

    /// Perform the WebSocket opening handshake over `stream`.
    ///
    /// Sends an HTTP/1.1 upgrade request to `GET {path}` on `{host}`, reads
    /// the server's response, and validates the `101 Switching Protocols`
    /// status and `Sec-WebSocket-Accept` header per RFC 6455 §4.
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
}

impl<S, R: Rng> WebSocket<S, R> {
    /// Returns a reference to the underlying stream.
    pub fn inner(&self) -> &S {
        &self.stream
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
        ws.sess.control_flood_score = 0;
        (ws, stream)
    }

    /// Set the maximum payload size accepted for a single frame or
    /// reassembled fragmented message.  Default: [`DEFAULT_MAX_PAYLOAD`].
    pub fn max_payload(mut self, max: usize) -> Self {
        assert!(
            max >= 125,
            "max_payload must be at least 125 (control frame limit)"
        );
        assert!(
            max <= isize::MAX as usize,
            "max_payload exceeds allocation limit"
        );
        self.sess.max_payload = max;
        self
    }

    /// Set the target buffer capacity.  After processing a large message
    /// the read buffer is shrunk back to this size.  Default:
    /// [`DEFAULT_MAX_BUF_SIZE`].  Must be at least 125.
    pub fn max_buf_size(mut self, max: usize) -> Self {
        assert!(
            max >= 125,
            "max_buf_size must be at least 125 (control frame payload limit)"
        );
        self.sess.max_buf_size = max;
        self
    }

    /// Set the maximum number of control frames (ping/pong) that
    /// [`read_message`](WebSocket::read_message) will process before
    /// returning `Ok(None)`.  Default: `usize::MAX` (unlimited).
    /// Must be at least 1.
    ///
    /// This gives the caller periodic control even when the server
    /// sends many pings without data frames.
    pub fn control_frame_budget(mut self, budget: usize) -> Self {
        assert!(budget >= 1, "control_frame_budget must be at least 1");
        self.sess.control_frame_budget = budget;
        self
    }

    /// Set the threshold for control frame flood detection.
    ///
    /// The score increments for each control frame processed and
    /// decrements for each small read (≤ 127 bytes) from the OS,
    /// resetting when a data message is delivered.  A burst of
    /// control frames drives the score up; a trickle of lone pings
    /// keeps it near zero.  Default: 1000.
    pub fn max_control_flood_score(mut self, max: usize) -> Self {
        self.sess.max_control_flood_score = max;
        self
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
