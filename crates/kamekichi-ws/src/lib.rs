#![forbid(unsafe_code)]

mod read_buf;
mod send_buf;

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

#[cfg(test)]
mod tests {
    use super::*;

    struct CounterRng(u32);

    impl rand_core::TryRng for CounterRng {
        type Error = std::convert::Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            let v = self.0;
            self.0 = self.0.wrapping_add(1);
            Ok(v)
        }

        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            let lo = self.try_next_u32()? as u64;
            let hi = self.try_next_u32()? as u64;
            Ok(lo | (hi << 32))
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
            for chunk in dest.chunks_mut(4) {
                let bytes = self.try_next_u32()?.to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            Ok(())
        }
    }
    use std::io::{self, Cursor};

    struct MockStream {
        rx: Cursor<Vec<u8>>,
        tx: Vec<u8>,
    }

    impl MockStream {
        fn new(data: Vec<u8>) -> Self {
            MockStream {
                rx: Cursor::new(data),
                tx: Vec::new(),
            }
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Build an unmasked server frame.
    fn frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(if fin { 0x80 } else { 0 } | opcode);
        let len = payload.len();
        if len < 126 {
            buf.push(len as u8);
        } else if len < 65536 {
            buf.push(126);
            buf.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            buf.push(127);
            buf.extend_from_slice(&(len as u64).to_be_bytes());
        }
        buf.extend_from_slice(payload);
        buf
    }

    /// Build a masked server frame.
    fn masked_frame(fin: bool, opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(if fin { 0x80 } else { 0 } | opcode);
        let len = payload.len();
        if len < 126 {
            buf.push(0x80 | len as u8);
        } else if len < 65536 {
            buf.push(0x80 | 126);
            buf.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            buf.push(0x80 | 127);
            buf.extend_from_slice(&(len as u64).to_be_bytes());
        }
        buf.extend_from_slice(&mask);
        for (i, &b) in payload.iter().enumerate() {
            buf.push(b ^ mask[i & 3]);
        }
        buf
    }

    fn ws(data: Vec<u8>) -> WebSocket<MockStream, CounterRng> {
        WebSocket::new(CounterRng(0)).with_stream(MockStream::new(data))
    }

    // ---- Length encodings ----

    #[test]
    fn text_7bit_length() {
        let mut ws = ws(frame(true, OP_TEXT, b"hello"));
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn text_16bit_length() {
        let payload = vec![b'x'; 200];
        let mut ws = ws(frame(true, OP_TEXT, &payload));
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => {
                assert_eq!(p.len(), 200);
                assert!(p.bytes().all(|b| b == b'x'));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn binary_64bit_length() {
        let payload = vec![0xAB; 65536];
        let mut ws = ws(frame(true, OP_BINARY, &payload));
        match ws.read_message().unwrap().unwrap() {
            Message::Binary(b) => {
                assert_eq!(b.len(), 65536);
                assert!(b.iter().all(|&x| x == 0xAB));
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn payload_too_large() {
        let mut data = vec![0x82, 127]; // FIN + binary, 64-bit length
        data.extend_from_slice(&(DEFAULT_MAX_PAYLOAD as u64 + 1).to_be_bytes());
        let mut ws = ws(data);
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::PayloadTooLarge(_)))
        ));
    }

    // ---- Masked server frames (RFC 6455 §5.1: MUST reject) ----

    #[test]
    fn masked_server_frame_rejected() {
        let mask = [0x12, 0x34, 0x56, 0x78];
        let mut ws = ws(masked_frame(true, OP_TEXT, b"hello", mask));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::MaskedServerFrame))
        ));
    }

    // ---- Binary ----

    #[test]
    fn binary_frame() {
        let payload = vec![0, 1, 2, 0xFF];
        let mut ws = ws(frame(true, OP_BINARY, &payload));
        match ws.read_message().unwrap().unwrap() {
            Message::Binary(b) => assert_eq!(b, &payload),
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    // ---- Control frames ----

    #[test]
    fn ping_replies_pong_then_returns_next() {
        let mut data = frame(true, OP_PING, b"ping-data");
        data.extend(frame(true, OP_TEXT, b"after"));
        let mut ws = ws(data);
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "after"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(!ws.inner().tx.is_empty(), "pong should have been written");
    }

    #[test]
    fn pong_ignored() {
        let mut data = frame(true, OP_PONG, b"");
        data.extend(frame(true, OP_TEXT, b"after"));
        let mut ws = ws(data);
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "after"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn close_empty_payload() {
        let mut ws = ws(frame(true, OP_CLOSE, b""));
        match ws.read_message().unwrap().unwrap() {
            Message::Close(code, reason) => {
                assert_eq!(code, None);
                assert!(reason.is_empty());
            }
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn close_with_code_and_reason() {
        let mut payload = 1000u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"normal");
        let mut ws = ws(frame(true, OP_CLOSE, &payload));
        match ws.read_message().unwrap().unwrap() {
            Message::Close(code, reason) => {
                assert_eq!(code, Some(1000));
                assert_eq!(reason, "normal");
            }
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn close_one_byte_is_error() {
        let mut ws = ws(frame(true, OP_CLOSE, b"x"));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::BadClosePayload))
        ));
    }

    #[test]
    fn fragmented_control_is_error() {
        let mut ws = ws(frame(false, OP_PING, b""));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::FragmentedControl))
        ));
    }

    #[test]
    fn control_payload_too_large() {
        let mut ws = ws(frame(true, OP_PING, &[0u8; 126]));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::ControlPayloadTooLarge(
                126
            )))
        ));
    }

    // ---- Fragmentation ----

    #[test]
    fn fragmented_text() {
        let mut data = frame(false, OP_TEXT, b"hel");
        data.extend(frame(false, OP_CONTINUATION, b"lo "));
        data.extend(frame(true, OP_CONTINUATION, b"world"));
        let mut ws = ws(data);
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "hello world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn fragmented_binary() {
        let mut data = frame(false, OP_BINARY, &[1, 2]);
        data.extend(frame(true, OP_CONTINUATION, &[3, 4]));
        let mut ws = ws(data);
        match ws.read_message().unwrap().unwrap() {
            Message::Binary(b) => assert_eq!(b, [1, 2, 3, 4]),
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn control_interleaved_in_fragments() {
        let mut data = frame(false, OP_TEXT, b"hel");
        data.extend(frame(true, OP_PING, b""));
        data.extend(frame(true, OP_CONTINUATION, b"lo"));
        let mut ws = ws(data);
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn unexpected_continuation() {
        let mut ws = ws(frame(true, OP_CONTINUATION, b"oops"));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::UnexpectedContinuation))
        ));
    }

    #[test]
    fn data_during_fragmentation() {
        let mut data = frame(false, OP_TEXT, b"start");
        data.extend(frame(true, OP_TEXT, b"new"));
        let mut ws = ws(data);
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::DataDuringFragmentation))
        ));
    }

    // ---- Error cases ----

    #[test]
    fn reserved_bits_set() {
        let mut ws = ws(vec![0x80 | 0x10 | OP_TEXT, 0]); // RSV1 set
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::BadReservedBits(_)))
        ));
    }

    #[test]
    fn unknown_opcode() {
        let mut ws = ws(vec![0x80 | 0x03, 0]); // opcode 3 is reserved
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::UnknownOpcode(3)))
        ));
    }

    #[test]
    fn invalid_utf8_text() {
        let mut ws = ws(frame(true, OP_TEXT, &[0xFF, 0xFE]));
        match ws.read_message() {
            Err(Error::Reconnect(ConnectionError::InvalidUtf8(_))) => {}
            other => panic!("expected InvalidUtf8, got {other:?}"),
        }
    }

    #[test]
    fn eof_mid_frame() {
        let mut data = vec![0x82, 100]; // FIN + binary, claims 100 bytes
        data.extend_from_slice(&[0; 5]); // only 5 follow
        let mut ws = ws(data);
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::Closed))
        ));
    }

    // ---- Compaction ----

    #[test]
    fn many_frames_survive_compaction() {
        // ~20KB of frames forces start past MAX_HEADER (8192), triggering compaction
        let payload = vec![b'a'; 1000];
        let mut data = Vec::new();
        for _ in 0..20 {
            data.extend(frame(true, OP_TEXT, &payload));
        }
        data.extend(frame(true, OP_TEXT, b"final"));

        let mut ws = ws(data);
        for _ in 0..20 {
            match ws.read_message().unwrap().unwrap() {
                Message::Text(p) => assert_eq!(p.len(), 1000),
                other => panic!("expected Text, got {other:?}"),
            }
        }
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "final"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn sequential_messages_all_correct() {
        let mut data = Vec::new();
        for i in 0..100u32 {
            data.extend(frame(true, OP_TEXT, format!("msg-{i}").as_bytes()));
        }
        let mut ws = ws(data);
        for i in 0..100u32 {
            match ws.read_message().unwrap().unwrap() {
                Message::Text(p) => assert_eq!(p, format!("msg-{i}")),
                other => panic!("expected Text, got {other:?}"),
            }
        }
    }

    // ---- Helpers for connect, send, and partial-read tests ----

    fn extract_ws_key(request: &[u8]) -> Vec<u8> {
        let needle = b"Sec-WebSocket-Key: ";
        let pos = request
            .windows(needle.len())
            .position(|w| w == needle.as_slice())
            .expect("request must contain Sec-WebSocket-Key");
        let start = pos + needle.len();
        let end = start + request[start..].iter().position(|&b| b == b'\r').unwrap();
        request[start..end].to_vec()
    }

    fn compute_accept(key: &[u8]) -> Vec<u8> {
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
        ctx.update(key);
        ctx.update(WS_GUID.as_bytes());
        let mut buf = [0u8; 28];
        base64::engine::general_purpose::STANDARD
            .encode_slice(ctx.finish().as_ref(), &mut buf)
            .unwrap();
        buf.to_vec()
    }

    fn parse_client_frame(data: &[u8]) -> (u8, Vec<u8>) {
        assert_eq!(data[0] & FIN, FIN, "FIN must be set");
        assert_eq!(data[1] & MASK_BIT, MASK_BIT, "MASK must be set");
        let opcode = data[0] & OPCODE_MASK;
        let len7 = (data[1] & LEN_MASK) as usize;
        let (hdr, payload_len) = match len7 {
            n @ 0..=125 => (2, n),
            126 => (4, u16::from_be_bytes([data[2], data[3]]) as usize),
            _ => (
                10,
                u64::from_be_bytes(data[2..10].try_into().unwrap()) as usize,
            ),
        };
        let mask: [u8; 4] = data[hdr..hdr + 4].try_into().unwrap();
        let mut payload = data[hdr + 4..hdr + 4 + payload_len].to_vec();
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
        (opcode, payload)
    }

    struct ConnectMock {
        tx: Vec<u8>,
        rx: Vec<u8>,
        rx_pos: usize,
        auto: bool,
        include_upgrade: bool,
        include_connection: bool,
        bare_lf: bool,
    }

    impl ConnectMock {
        fn auto() -> Self {
            ConnectMock {
                tx: Vec::new(),
                rx: Vec::new(),
                rx_pos: 0,
                auto: true,
                include_upgrade: true,
                include_connection: true,
                bare_lf: false,
            }
        }
        fn auto_bare_lf() -> Self {
            ConnectMock {
                tx: Vec::new(),
                rx: Vec::new(),
                rx_pos: 0,
                auto: true,
                include_upgrade: true,
                include_connection: true,
                bare_lf: true,
            }
        }
        fn auto_with_extra(extra: Vec<u8>) -> Self {
            ConnectMock {
                tx: Vec::new(),
                rx: extra,
                rx_pos: 0,
                auto: true,
                include_upgrade: true,
                include_connection: true,
                bare_lf: false,
            }
        }
        fn auto_without_upgrade() -> Self {
            ConnectMock {
                tx: Vec::new(),
                rx: Vec::new(),
                rx_pos: 0,
                auto: true,
                include_upgrade: false,
                include_connection: true,
                bare_lf: false,
            }
        }
        fn auto_without_connection() -> Self {
            ConnectMock {
                tx: Vec::new(),
                rx: Vec::new(),
                rx_pos: 0,
                auto: true,
                include_upgrade: true,
                include_connection: false,
                bare_lf: false,
            }
        }
        fn with_response(rx: Vec<u8>) -> Self {
            ConnectMock {
                tx: Vec::new(),
                rx,
                rx_pos: 0,
                auto: false,
                include_upgrade: true,
                include_connection: true,
                bare_lf: false,
            }
        }
    }

    impl Read for ConnectMock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.auto {
                let key = extract_ws_key(&self.tx);
                let accept = compute_accept(&key);
                let accept_str = std::str::from_utf8(&accept).unwrap();
                let extra = std::mem::take(&mut self.rx);
                let nl = if self.bare_lf { "\n" } else { "\r\n" };
                let mut response = format!("HTTP/1.1 101 Switching Protocols{nl}");
                if self.include_upgrade {
                    response.push_str(&format!("Upgrade: websocket{nl}"));
                }
                if self.include_connection {
                    response.push_str(&format!("Connection: Upgrade{nl}"));
                }
                response.push_str(&format!("Sec-WebSocket-Accept: {accept_str}{nl}{nl}"));
                self.rx = response.into_bytes();
                self.rx.extend(extra);
                self.auto = false;
            }
            let remaining = &self.rx[self.rx_pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.rx_pos += n;
            Ok(n)
        }
    }

    impl Write for ConnectMock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ChunkedMock {
        rx: Cursor<Vec<u8>>,
        tx: Vec<u8>,
        chunk_size: usize,
        blocks: usize,
    }

    impl ChunkedMock {
        fn new(data: Vec<u8>, chunk_size: usize) -> Self {
            ChunkedMock {
                rx: Cursor::new(data),
                tx: Vec::new(),
                chunk_size,
                blocks: 0,
            }
        }

        fn with_blocks(data: Vec<u8>, chunk_size: usize, blocks: usize) -> Self {
            ChunkedMock {
                rx: Cursor::new(data),
                tx: Vec::new(),
                chunk_size,
                blocks,
            }
        }
    }

    impl Read for ChunkedMock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.blocks > 0 {
                self.blocks -= 1;
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"));
            }
            let limit = buf.len().min(self.chunk_size);
            self.rx.read(&mut buf[..limit])
        }
    }

    impl Write for ChunkedMock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // ---- Connect ----

    #[test]
    fn connect_success() {
        let _ws = WebSocket::new(CounterRng(0))
            .connect(ConnectMock::auto(), "example.com", "/ws")
            .unwrap();
    }

    #[test]
    fn connect_bare_lf_headers() {
        let _ws = WebSocket::new(CounterRng(0))
            .connect(ConnectMock::auto_bare_lf(), "example.com", "/ws")
            .unwrap();
    }

    #[test]
    fn connect_preserves_leftover() {
        let extra = frame(true, OP_TEXT, b"leftover");
        let mut ws = WebSocket::new(CounterRng(0))
            .connect(ConnectMock::auto_with_extra(extra), "example.com", "/ws")
            .unwrap();
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "leftover"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn connect_bad_status() {
        let rx = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec();
        assert!(matches!(
            WebSocket::new(CounterRng(0)).connect(
                ConnectMock::with_response(rx),
                "example.com",
                "/"
            ),
            Err(Error::Reconnect(ConnectionError::BadStatus))
        ));
    }

    #[test]
    fn connect_missing_accept() {
        let rx = b"HTTP/1.1 101 Switching Protocols\r\n\
                    Upgrade: websocket\r\n\
                    \r\n"
            .to_vec();
        assert!(matches!(
            WebSocket::new(CounterRng(0)).connect(
                ConnectMock::with_response(rx),
                "example.com",
                "/"
            ),
            Err(Error::Reconnect(ConnectionError::MissingAccept))
        ));
    }

    #[test]
    fn connect_bad_accept() {
        let rx = b"HTTP/1.1 101 Switching Protocols\r\n\
                    Sec-WebSocket-Accept: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                    \r\n"
            .to_vec();
        assert!(matches!(
            WebSocket::new(CounterRng(0)).connect(
                ConnectMock::with_response(rx),
                "example.com",
                "/"
            ),
            Err(Error::Reconnect(ConnectionError::BadAccept))
        ));
    }

    #[test]
    fn connect_headers_too_large() {
        let rx = vec![b'X'; 8300];
        assert!(matches!(
            WebSocket::new(CounterRng(0)).connect(
                ConnectMock::with_response(rx),
                "example.com",
                "/"
            ),
            Err(Error::Reconnect(ConnectionError::HeadersTooLarge))
        ));
    }

    #[test]
    fn connect_missing_upgrade() {
        assert!(matches!(
            WebSocket::new(CounterRng(0)).connect(
                ConnectMock::auto_without_upgrade(),
                "example.com",
                "/"
            ),
            Err(Error::Reconnect(ConnectionError::MissingUpgrade))
        ));
    }

    #[test]
    fn connect_missing_connection() {
        assert!(matches!(
            WebSocket::new(CounterRng(0)).connect(
                ConnectMock::auto_without_connection(),
                "example.com",
                "/"
            ),
            Err(Error::Reconnect(ConnectionError::MissingConnection))
        ));
    }

    #[test]
    fn connect_eof_mid_headers() {
        let rx = b"HTTP/1.1 101 Switch".to_vec();
        assert!(matches!(
            WebSocket::new(CounterRng(0)).connect(
                ConnectMock::with_response(rx),
                "example.com",
                "/"
            ),
            Err(Error::Reconnect(ConnectionError::Closed))
        ));
    }

    // ---- Send frame verification ----

    #[test]
    fn send_text_verified() {
        let mut ws = ws(Vec::new());
        ws.send_text("hello").unwrap();
        let (opcode, payload) = parse_client_frame(&ws.inner().tx);
        assert_eq!(opcode, OP_TEXT);
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn send_text_16bit_verified() {
        let text: String = "x".repeat(200);
        let mut ws = ws(Vec::new());
        ws.send_text(&text).unwrap();
        let (opcode, payload) = parse_client_frame(&ws.inner().tx);
        assert_eq!(opcode, OP_TEXT);
        assert_eq!(payload.len(), 200);
    }

    #[test]
    fn close_echo_verified() {
        let mut body = 1000u16.to_be_bytes().to_vec();
        body.extend_from_slice(b"goodbye");
        let mut ws = ws(frame(true, OP_CLOSE, &body));
        let _ = ws.read_message().unwrap().unwrap();
        let (opcode, payload) = parse_client_frame(&ws.inner().tx);
        assert_eq!(opcode, OP_CLOSE);
        assert_eq!(payload, 1000u16.to_be_bytes());
    }

    #[test]
    fn ping_reply_verified() {
        let mut data = frame(true, OP_PING, b"ping-data");
        data.extend(frame(true, OP_TEXT, b"x"));
        let mut ws = ws(data);
        let _ = ws.read_message().unwrap().unwrap();
        let (opcode, payload) = parse_client_frame(&ws.inner().tx);
        assert_eq!(opcode, OP_PONG);
        assert_eq!(payload, b"ping-data");
    }

    // ---- Close code validation ----

    #[test]
    fn send_close_valid_code() {
        let mut ws = ws(Vec::new());
        ws.send_close(1000, "normal").unwrap();
    }

    #[test]
    fn send_close_rejects_zero() {
        let mut ws = ws(Vec::new());
        assert!(matches!(
            ws.send_close(0, ""),
            Err(Error::Fatal(CallerError::InvalidCloseCode(0)))
        ));
    }

    #[test]
    fn send_close_rejects_low_codes() {
        let mut ws = ws(Vec::new());
        assert!(matches!(
            ws.send_close(999, ""),
            Err(Error::Fatal(CallerError::InvalidCloseCode(999)))
        ));
    }

    #[test]
    fn send_close_rejects_1005() {
        let mut ws = ws(Vec::new());
        assert!(matches!(
            ws.send_close(1005, ""),
            Err(Error::Fatal(CallerError::InvalidCloseCode(1005)))
        ));
    }

    #[test]
    fn send_close_rejects_1006() {
        let mut ws = ws(Vec::new());
        assert!(matches!(
            ws.send_close(1006, ""),
            Err(Error::Fatal(CallerError::InvalidCloseCode(1006)))
        ));
    }

    #[test]
    fn send_close_rejects_1015() {
        let mut ws = ws(Vec::new());
        assert!(matches!(
            ws.send_close(1015, ""),
            Err(Error::Fatal(CallerError::InvalidCloseCode(1015)))
        ));
    }

    #[test]
    fn send_close_allows_private_use() {
        let mut ws = ws(Vec::new());
        ws.send_close(4000, "private").unwrap();
    }

    // ---- Partial reads ----

    #[test]
    fn partial_read_one_byte_at_a_time() {
        let data = frame(true, OP_TEXT, b"hello world");
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(ChunkedMock::new(data, 1));
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "hello world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn partial_read_16bit_frame() {
        let payload = vec![b'y'; 300];
        let data = frame(true, OP_TEXT, &payload);
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(ChunkedMock::new(data, 3));
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => {
                assert_eq!(p.len(), 300);
                assert!(p.bytes().all(|b| b == b'y'));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn partial_read_masked_frame_rejected() {
        let mask = [0xAA, 0xBB, 0xCC, 0xDD];
        let data = masked_frame(true, OP_TEXT, b"chunked", mask);
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(ChunkedMock::new(data, 1));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::MaskedServerFrame))
        ));
    }

    // ---- WouldBlock recovery ----

    #[test]
    fn would_block_then_read_succeeds() {
        // fill_at_least resizes the buffer before calling read(). If read()
        // returns WouldBlock, the zero-padding must be truncated so the next
        // read_message call sees a clean buffer.
        let data = frame(true, OP_TEXT, b"hello");
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(ChunkedMock::with_blocks(
            data,
            usize::MAX,
            1,
        ));
        assert!(matches!(ws.read_message(), Ok(None)));
        match ws.read_message().unwrap().unwrap() {
            Message::Text(p) => assert_eq!(p, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ---- Write-blocking mock for send buffer tests ----

    struct WriteBlockingMock {
        rx: Cursor<Vec<u8>>,
    }

    impl WriteBlockingMock {
        fn new(data: Vec<u8>) -> Self {
            WriteBlockingMock {
                rx: Cursor::new(data),
            }
        }
    }

    impl Read for WriteBlockingMock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for WriteBlockingMock {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "blocked"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // ---- Flood detection ----

    #[test]
    fn control_frame_flood_detected() {
        // All pings pre-buffered → large initial read, then bytes_read=0
        // for subsequent frames → no score decrement → score climbs.
        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend(frame(true, OP_PING, b""));
        }
        let mut ws = WebSocket::new(CounterRng(0))
            .max_control_flood_score(3)
            .with_stream(MockStream::new(data));

        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::ControlFlood))
        ));
    }

    // ---- Pong suppression under buffer pressure ----

    #[test]
    fn pong_suppressed_when_send_buffer_full() {
        // Writes are blocked, so the first pong stays in the send buffer.
        // With max_buf_size=125, subsequent pings have their pongs
        // suppressed (pending_len >= max_buf_size).
        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend(frame(true, OP_PING, b""));
        }
        data.extend(frame(true, OP_TEXT, b"done"));

        let mut ws = WebSocket::new(CounterRng(0))
            .max_buf_size(125)
            .with_stream(WriteBlockingMock::new(data));

        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t, "done"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ---- Fragmented message too large ----

    #[test]
    fn fragmented_message_too_large() {
        let mut data = Vec::new();
        data.extend(frame(false, OP_TEXT, &[b'a'; 100]));
        data.extend(frame(true, OP_CONTINUATION, &[b'b'; 100]));

        let mut ws = WebSocket::new(CounterRng(0))
            .max_payload(150)
            .with_stream(MockStream::new(data));

        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(
                ConnectionError::FragmentedMessageTooLarge(200)
            ))
        ));
    }

    // ---- Wire encoding validation ----

    #[test]
    fn non_minimal_16bit_length() {
        // Payload of 50 bytes encoded with 16-bit length (should use 7-bit)
        let mut data = Vec::new();
        data.push(0x81); // FIN + text
        data.push(126); // 16-bit length follows
        data.extend_from_slice(&50u16.to_be_bytes());
        data.extend_from_slice(&[b'x'; 50]);

        let mut ws = ws(data);
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::NonMinimalLength))
        ));
    }

    #[test]
    fn non_minimal_64bit_length() {
        // Payload of 200 bytes encoded with 64-bit length (should use 16-bit)
        let mut data = Vec::new();
        data.push(0x81); // FIN + text
        data.push(127); // 64-bit length follows
        data.extend_from_slice(&200u64.to_be_bytes());
        data.extend_from_slice(&[b'x'; 200]);

        let mut ws = ws(data);
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::NonMinimalLength))
        ));
    }

    #[test]
    fn payload_length_msb_set() {
        let mut data = Vec::new();
        data.push(0x82); // FIN + binary
        data.push(127); // 64-bit length follows
        data.extend_from_slice(&(1u64 << 63).to_be_bytes());
        // No payload bytes needed — error triggers before payload read

        let mut ws = ws(data);
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::PayloadLengthMsb))
        ));
    }

    // ---- Close code validation (receive) ----

    #[test]
    fn recv_close_code_1005_rejected() {
        let mut ws = ws(frame(true, OP_CLOSE, &1005u16.to_be_bytes()));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::InvalidCloseCode(1005)))
        ));
    }

    #[test]
    fn recv_close_code_1006_rejected() {
        let mut ws = ws(frame(true, OP_CLOSE, &1006u16.to_be_bytes()));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::InvalidCloseCode(1006)))
        ));
    }

    #[test]
    fn recv_close_code_1015_rejected() {
        let mut ws = ws(frame(true, OP_CLOSE, &1015u16.to_be_bytes()));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::InvalidCloseCode(1015)))
        ));
    }

    // ---- Control frame budget ----

    #[test]
    fn control_frame_budget_returns_none() {
        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend(frame(true, OP_PING, b""));
        }
        data.extend(frame(true, OP_TEXT, b"after"));

        let mut ws = WebSocket::new(CounterRng(0))
            .control_frame_budget(3)
            .with_stream(MockStream::new(data));

        // First call: processes 3 pings, budget exhausted
        assert!(matches!(ws.read_message(), Ok(None)));

        // Second call: processes remaining 2 pings, then returns text
        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t, "after"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ---- try_connect recovery ----

    #[test]
    fn try_connect_recovers_on_handshake_failure() {
        let rx = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec();
        let ws = WebSocket::new(CounterRng(0));
        let Err((err, ws, _stream)) =
            ws.try_connect(ConnectMock::with_response(rx), "example.com", "/")
        else {
            panic!("expected error");
        };
        assert!(matches!(err, Error::Reconnect(ConnectionError::BadStatus)));
        // Recovered WebSocket can be reused for a new connection.
        let _ws = ws
            .connect(ConnectMock::auto(), "example.com", "/ws")
            .unwrap();
    }

    #[test]
    fn try_connect_recovers_on_invalid_header() {
        let ws = WebSocket::new(CounterRng(0));
        let Err((err, ws, _stream)) = ws.try_connect(ConnectMock::auto(), "bad\nhost", "/") else {
            panic!("expected error");
        };
        assert!(matches!(err, Error::Fatal(CallerError::InvalidHeaderValue)));
        // Stream was never touched; WebSocket reusable.
        let _ws = ws
            .connect(ConnectMock::auto(), "example.com", "/ws")
            .unwrap();
    }

    #[test]
    fn try_connect_invalid_path() {
        let ws = WebSocket::new(CounterRng(0));
        let Err((err, _ws, _stream)) =
            ws.try_connect(ConnectMock::auto(), "example.com", "/bad\npath")
        else {
            panic!("expected error");
        };
        assert!(matches!(err, Error::Fatal(CallerError::InvalidHeaderValue)));
    }

    // ---- Display / Error trait coverage ----

    fn make_utf8_error() -> std::str::Utf8Error {
        std::str::from_utf8(std::hint::black_box(&[0xFF])).unwrap_err()
    }

    #[test]
    fn connection_error_display() {
        let cases: &[ConnectionError] = &[
            ConnectionError::Io(io::Error::other("test")),
            ConnectionError::Closed,
            ConnectionError::HeadersTooLarge,
            ConnectionError::BadStatus,
            ConnectionError::MissingAccept,
            ConnectionError::BadAccept,
            ConnectionError::BadReservedBits(0x10),
            ConnectionError::PayloadTooLarge(999),
            ConnectionError::UnexpectedContinuation,
            ConnectionError::FragmentedMessageTooLarge(999),
            ConnectionError::InvalidUtf8(make_utf8_error()),
            ConnectionError::DataDuringFragmentation,
            ConnectionError::FragmentedControl,
            ConnectionError::ControlPayloadTooLarge(200),
            ConnectionError::BadClosePayload,
            ConnectionError::UnknownOpcode(0x03),
            ConnectionError::NonMinimalLength,
            ConnectionError::PayloadLengthMsb,
            ConnectionError::MissingUpgrade,
            ConnectionError::MissingConnection,
            ConnectionError::InvalidCloseCode(9999),
            ConnectionError::MaskedServerFrame,
            ConnectionError::ControlFlood,
        ];
        for e in cases {
            assert!(!e.to_string().is_empty());
        }
    }

    #[test]
    fn connection_error_source() {
        let io_err = ConnectionError::Io(io::Error::other("t"));
        assert!(std::error::Error::source(&io_err).is_some());
        let utf8_err = ConnectionError::InvalidUtf8(make_utf8_error());
        assert!(std::error::Error::source(&utf8_err).is_some());
        let other = ConnectionError::Closed;
        assert!(std::error::Error::source(&other).is_none());
    }

    #[test]
    fn caller_error_display() {
        let cases: &[CallerError] = &[
            CallerError::Closing,
            CallerError::InvalidHeaderValue,
            CallerError::InvalidCloseCode(999),
            CallerError::CloseReasonTooLong(200),
        ];
        for e in cases {
            assert!(!e.to_string().is_empty());
        }
        // std::error::Error is implemented
        assert!(std::error::Error::source(&CallerError::Closing).is_none());
    }

    #[test]
    fn error_display_and_source() {
        let r = Error::Reconnect(ConnectionError::Closed);
        assert!(!r.to_string().is_empty());
        assert!(std::error::Error::source(&r).is_some());
        let f = Error::Fatal(CallerError::Closing);
        assert!(!f.to_string().is_empty());
        assert!(std::error::Error::source(&f).is_some());
    }

    #[test]
    fn from_impls() {
        // From<Utf8Error> for ConnectionError
        let _: ConnectionError = make_utf8_error().into();
        // From<FillError::BufferFull> for ConnectionError
        let e: ConnectionError = FillError::BufferFull.into();
        assert!(matches!(e, ConnectionError::HeadersTooLarge));
        // From<io::Error> for Error
        let _: Error = io::Error::other("t").into();
        // From<Utf8Error> for Error
        let _: Error = make_utf8_error().into();
    }

    // ---- read_message after close ----

    #[test]
    fn read_message_after_close_is_fatal() {
        let mut ws = ws(frame(true, OP_CLOSE, b""));
        ws.read_message().unwrap(); // consumes close
        assert!(matches!(
            ws.read_message(),
            Err(Error::Fatal(CallerError::Closing))
        ));
    }

    // ---- send_frame after close ----

    #[test]
    fn send_after_close_is_fatal() {
        let mut ws = ws(Vec::new());
        ws.send_close(1000, "").unwrap();
        assert!(matches!(
            ws.send_text("x"),
            Err(Error::Fatal(CallerError::Closing))
        ));
    }

    // ---- send_binary ----

    #[test]
    fn send_binary_verified() {
        let mut ws = ws(Vec::new());
        ws.send_binary(b"data").unwrap();
        let (opcode, payload) = parse_client_frame(&ws.inner().tx);
        assert_eq!(opcode, OP_BINARY);
        assert_eq!(payload, b"data");
    }

    // ---- send_close reason too long ----

    #[test]
    fn send_close_reason_too_long() {
        let mut ws = ws(Vec::new());
        let reason = "x".repeat(124);
        assert!(matches!(
            ws.send_close(1000, &reason),
            Err(Error::Fatal(CallerError::CloseReasonTooLong(124)))
        ));
    }

    // ---- recv close code out of range ----

    #[test]
    fn recv_close_code_out_of_range() {
        let mut ws = ws(frame(true, OP_CLOSE, &5000u16.to_be_bytes()));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::InvalidCloseCode(5000)))
        ));
    }

    // ---- build_frame 64-bit length ----

    #[test]
    fn send_binary_64bit_length() {
        let payload = vec![0u8; 65536];
        let mut ws = ws(Vec::new());
        ws.send_binary(&payload).unwrap();
        let (opcode, decoded) = parse_client_frame(&ws.inner().tx);
        assert_eq!(opcode, OP_BINARY);
        assert_eq!(decoded.len(), 65536);
    }

    // ---- data frame exceeds max_payload (non-64bit path) ----

    #[test]
    fn data_frame_exceeds_max_payload() {
        let payload = vec![b'x'; 126];
        let mut ws = WebSocket::new(CounterRng(0))
            .max_payload(125)
            .with_stream(MockStream::new(frame(true, OP_TEXT, &payload)));
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::PayloadTooLarge(126)))
        ));
    }

    // ---- builder asserts ----

    #[test]
    #[should_panic(expected = "max_payload must be at least 125")]
    fn max_payload_too_small() {
        WebSocket::new(CounterRng(0)).max_payload(124);
    }

    #[test]
    #[should_panic(expected = "max_payload exceeds allocation limit")]
    fn max_payload_too_large() {
        WebSocket::new(CounterRng(0)).max_payload(isize::MAX as usize + 1);
    }

    #[test]
    #[should_panic(expected = "max_buf_size must be at least 125")]
    fn max_buf_size_too_small() {
        WebSocket::new(CounterRng(0)).max_buf_size(124);
    }

    // ---- Additional mocks for send/flush paths ----

    /// Write blocks first N calls, then accepts.
    struct BlockFirstNWritesMock {
        rx: Cursor<Vec<u8>>,
        tx: Vec<u8>,
        remaining_blocks: usize,
    }

    impl Read for BlockFirstNWritesMock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for BlockFirstNWritesMock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining_blocks > 0 {
                self.remaining_blocks -= 1;
                Err(io::Error::new(io::ErrorKind::WouldBlock, "blocked"))
            } else {
                self.tx.extend_from_slice(buf);
                Ok(buf.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Write blocks first N calls, then returns hard error.
    struct BlockThenErrorMock {
        rx: Cursor<Vec<u8>>,
        blocks: usize,
    }

    impl Read for BlockThenErrorMock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for BlockThenErrorMock {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            if self.blocks > 0 {
                self.blocks -= 1;
                Err(io::Error::new(io::ErrorKind::WouldBlock, "blocked"))
            } else {
                Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Write returns Ok(0) (simulating a closed stream).
    struct WriteZeroMock;

    impl Read for WriteZeroMock {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for WriteZeroMock {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Write returns a hard error.
    struct WriteErrorMock;

    impl Read for WriteErrorMock {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for WriteErrorMock {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Writes succeed, but flush() returns WouldBlock.
    struct StreamFlushBlocksMock {
        rx: Cursor<Vec<u8>>,
        tx: Vec<u8>,
    }

    impl Read for StreamFlushBlocksMock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for StreamFlushBlocksMock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "flush blocked"))
        }
    }

    /// Writes succeed, but flush() returns a hard error.
    struct StreamFlushErrorMock {
        rx: Cursor<Vec<u8>>,
        tx: Vec<u8>,
    }

    impl Read for StreamFlushErrorMock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for StreamFlushErrorMock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset"))
        }
    }

    /// Read returns Interrupted once, then reads normally.
    struct InterruptOnceThenRead {
        inner: Cursor<Vec<u8>>,
        interrupt: bool,
    }

    impl Read for InterruptOnceThenRead {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.interrupt {
                self.interrupt = false;
                Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"))
            } else {
                self.inner.read(buf)
            }
        }
    }

    /// Write returns Interrupted once, then writes normally.
    struct InterruptOnceWriter {
        rx: Cursor<Vec<u8>>,
        tx: Vec<u8>,
        interrupt: bool,
    }

    impl Read for InterruptOnceWriter {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for InterruptOnceWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.interrupt {
                self.interrupt = false;
                Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"))
            } else {
                self.tx.extend_from_slice(buf);
                Ok(buf.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // ---- Send/flush path tests ----

    #[test]
    fn send_queued_then_retry_later() {
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(WriteBlockingMock::new(Vec::new()));
        // First send: builds frame, flush blocks → Queued
        assert_eq!(ws.send_text("a").unwrap(), SendResult::Queued);
        // Second send: has_pending, flush blocks → RetryLater
        assert_eq!(ws.send_text("b").unwrap(), SendResult::RetryLater);
    }

    #[test]
    fn send_pending_flush_hard_error() {
        let stream = BlockThenErrorMock {
            rx: Cursor::new(Vec::new()),
            blocks: 1,
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        // First send: flush blocks → Queued
        assert_eq!(ws.send_text("a").unwrap(), SendResult::Queued);
        // Second send: has_pending, flush → ConnectionReset
        assert!(matches!(
            ws.send_text("b"),
            Err(Error::Reconnect(ConnectionError::Io(_)))
        ));
    }

    #[test]
    fn send_frame_flush_hard_error() {
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(WriteErrorMock);
        assert!(matches!(
            ws.send_text("hello"),
            Err(Error::Reconnect(ConnectionError::Io(_)))
        ));
    }

    #[test]
    fn send_write_zero_is_closed() {
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(WriteZeroMock);
        assert!(matches!(
            ws.send_text("hello"),
            Err(Error::Reconnect(ConnectionError::Closed))
        ));
    }

    #[test]
    fn send_interrupted_retries() {
        let stream = InterruptOnceWriter {
            rx: Cursor::new(Vec::new()),
            tx: Vec::new(),
            interrupt: true,
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        assert_eq!(ws.send_text("hello").unwrap(), SendResult::Done);
    }

    // ---- WebSocket::flush paths ----

    #[test]
    fn flush_no_pending() {
        let mut ws = ws(Vec::new());
        assert_eq!(ws.flush().unwrap(), SendResult::Done);
    }

    #[test]
    fn flush_clears_pending() {
        let stream = BlockFirstNWritesMock {
            rx: Cursor::new(Vec::new()),
            tx: Vec::new(),
            remaining_blocks: 1,
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        assert_eq!(ws.send_text("hello").unwrap(), SendResult::Queued);
        assert_eq!(ws.flush().unwrap(), SendResult::Done);
    }

    #[test]
    fn flush_pending_blocked() {
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(WriteBlockingMock::new(Vec::new()));
        assert_eq!(ws.send_text("hello").unwrap(), SendResult::Queued);
        assert_eq!(ws.flush().unwrap(), SendResult::Queued);
    }

    #[test]
    fn flush_pending_hard_error() {
        let stream = BlockThenErrorMock {
            rx: Cursor::new(Vec::new()),
            blocks: 1,
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        assert_eq!(ws.send_text("a").unwrap(), SendResult::Queued);
        assert!(matches!(
            ws.flush(),
            Err(Error::Reconnect(ConnectionError::Io(_)))
        ));
    }

    #[test]
    fn flush_stream_flush_would_block() {
        let stream = StreamFlushBlocksMock {
            rx: Cursor::new(Vec::new()),
            tx: Vec::new(),
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        // No pending send data, goes straight to stream.flush() → WouldBlock
        assert_eq!(ws.flush().unwrap(), SendResult::Queued);
    }

    #[test]
    fn flush_stream_flush_hard_error() {
        let stream = StreamFlushErrorMock {
            rx: Cursor::new(Vec::new()),
            tx: Vec::new(),
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        assert!(matches!(
            ws.flush(),
            Err(Error::Reconnect(ConnectionError::Io(_)))
        ));
    }

    // ---- Pending send during read_message ----

    #[test]
    fn pending_send_flushed_on_next_read() {
        let mut data = frame(true, OP_PING, b"p");
        data.extend(frame(true, OP_TEXT, b"a"));
        data.extend(frame(true, OP_TEXT, b"b"));
        let stream = BlockFirstNWritesMock {
            rx: Cursor::new(data),
            tx: Vec::new(),
            remaining_blocks: 1,
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        // First read: ping → pong queued (write blocked), returns text "a"
        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t, "a"),
            other => panic!("expected Text, got {other:?}"),
        }
        // Second read: pending pong flushed (write succeeds), returns text "b"
        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t, "b"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn pending_send_would_block_during_read() {
        let mut data = frame(true, OP_PING, b"p");
        data.extend(frame(true, OP_TEXT, b"a"));
        data.extend(frame(true, OP_TEXT, b"b"));
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(WriteBlockingMock::new(data));
        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t, "a"),
            other => panic!("expected Text, got {other:?}"),
        }
        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t, "b"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // ---- Fragment buffer shrinking ----

    #[test]
    fn fragment_buf_shrinks_after_large_message() {
        let mut data = frame(false, OP_TEXT, &[b'a'; 200]);
        data.extend(frame(true, OP_CONTINUATION, &[b'b'; 200]));
        // Need a third message so the second read_message call enters the
        // loop and hits the fragment_buf shrink check.
        data.extend(frame(true, OP_TEXT, b"done"));
        // Use max_buf_size=125 so fragment_buf (capacity ~400) > max_buf_size.
        // CounterRng(7): build mask uses rng(7), then the shrink checks use
        // rng(8)=8 which passes &0b111==0.  But there are no mask calls here
        // (no sends), so we need the rng call in maybe_shrink_capacity and
        // fragment_buf check to land on multiples of 8.
        // CounterRng(0): first rng call is maybe_shrink_capacity(0→0&7=0),
        // then fragment check rng(1→1&7≠0).  Won't trigger.
        // CounterRng(7): first rng call(7→7&7≠0) skip shrink_capacity,
        // fragment check rng(8→8&7=0) triggers!  But maybe_compact/shrink
        // may or may not call rng depending on conditions.
        // Simplest: use a Rng that always returns 0.
        let mut ws = WebSocket::new(ZeroRng)
            .max_buf_size(125)
            .with_stream(MockStream::new(data));
        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t.len(), 400),
            other => panic!("expected Text, got {other:?}"),
        }
        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t, "done"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    /// RNG that always returns 0 — deterministic for shrink tests.
    struct ZeroRng;

    impl rand_core::TryRng for ZeroRng {
        type Error = std::convert::Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            Ok(0)
        }
        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            Ok(0)
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
            dest.fill(0);
            Ok(())
        }
    }

    // ---- send_buf::maybe_shrink ----

    #[test]
    fn send_buf_shrinks_after_large_send() {
        // CounterRng(7): mask call returns 7 (counter→8),
        // maybe_shrink call returns 8 (8&7=0 → triggers shrink).
        let mut ws = WebSocket::new(CounterRng(7))
            .max_buf_size(125)
            .with_stream(MockStream::new(Vec::new()));
        ws.send_text("hello").unwrap();
        // The send buffer started at capacity 256, which exceeds max_buf_size=125.
        // maybe_shrink should have triggered.
    }

    // ---- send_buf::flush write returns 0 ----

    #[test]
    fn send_buf_flush_write_zero() {
        let mut send = SendBuf::with_capacity(16);
        send.push(b"hello");
        let mut stream = WriteZeroMock;
        assert!(matches!(
            send.flush(&mut stream),
            Err(ConnectionError::Closed)
        ));
    }

    // ---- read_buf::read_until coverage ----

    #[test]
    fn read_until_pre_existing_data() {
        let mut buf = ReadBuf::with_capacity(64);
        // Fill buffer with some data first.
        buf.fill_from(&mut Cursor::new(b"hello world".to_vec()), 5)
            .unwrap();
        buf.consume(2); // pending = "llo world..." (start=2, end≥5)
        // read_until sees pre-existing data (end > start) and the callback
        // returns immediately.
        let result = buf
            .read_until::<_, FillError>(&mut Cursor::new(Vec::new()), 64, |b| {
                if b.pending().len() >= 3 {
                    Ok(Some(b.pending().len()))
                } else {
                    Ok(None)
                }
            })
            .unwrap();
        assert!(result >= 3);
    }

    #[test]
    fn read_until_buffer_full() {
        let mut buf = ReadBuf::with_capacity(16);
        let data = vec![b'x'; 100];
        let result = buf.read_until::<(), FillError>(
            &mut Cursor::new(data),
            16,
            |_| Ok(None), // never accept
        );
        assert!(matches!(result, Err(FillError::BufferFull)));
    }

    #[test]
    fn read_until_interrupted() {
        let mut buf = ReadBuf::with_capacity(64);
        let mut reader = InterruptOnceThenRead {
            inner: Cursor::new(b"hello".to_vec()),
            interrupt: true,
        };
        buf.read_until::<_, FillError>(&mut reader, 64, |b| {
            if b.pending().len() >= 5 {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        })
        .unwrap();
    }

    #[test]
    fn read_until_io_error() {
        let mut buf = ReadBuf::with_capacity(64);
        struct ErrorReader;
        impl Read for ErrorReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset"))
            }
        }
        let result = buf.read_until::<(), FillError>(&mut ErrorReader, 64, |_| Ok(None));
        assert!(matches!(result, Err(FillError::Io(_))));
    }

    // ---- Pending send hard error during read_message ----

    #[test]
    fn pending_send_hard_error_during_read() {
        let mut data = frame(true, OP_PING, b"p");
        data.extend(frame(true, OP_TEXT, b"a"));
        data.extend(frame(true, OP_TEXT, b"b"));
        let stream = BlockThenErrorMock {
            rx: Cursor::new(data),
            blocks: 1,
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        // First read: ping → pong queued (write blocked), returns text "a"
        match ws.read_message().unwrap().unwrap() {
            Message::Text(t) => assert_eq!(t, "a"),
            other => panic!("expected Text, got {other:?}"),
        }
        // Second read: pending pong flush → hard error
        assert!(matches!(
            ws.read_message(),
            Err(Error::Reconnect(ConnectionError::Io(_)))
        ));
    }

    // ---- send_frame: pending flush succeeds then new frame sent ----

    #[test]
    fn send_pending_flush_succeeds_then_sends() {
        let stream = BlockFirstNWritesMock {
            rx: Cursor::new(Vec::new()),
            tx: Vec::new(),
            remaining_blocks: 1,
        };
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(stream);
        assert_eq!(ws.send_text("a").unwrap(), SendResult::Queued);
        // Pending data from "a" is flushed (write now succeeds), then "b" is built + sent
        assert_eq!(ws.send_text("b").unwrap(), SendResult::Done);
    }

    // ---- flush: stream.flush TimedOut variant ----

    #[test]
    fn flush_stream_flush_timed_out() {
        struct TimedOutFlushMock;
        impl Read for TimedOutFlushMock {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        impl Write for TimedOutFlushMock {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"))
            }
        }
        let mut ws = WebSocket::new(CounterRng(0)).with_stream(TimedOutFlushMock);
        assert_eq!(ws.flush().unwrap(), SendResult::Queued);
    }
}
