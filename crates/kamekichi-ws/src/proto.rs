use std::io::{Read, Write};
use std::time::Instant;

use base64::Engine;

use crate::error::{CallerError, ConnectionError as ConnError};
use crate::read_buf::ReadBuf;
use crate::rng::Rng;
use crate::send_buf::SendBuf;
use crate::{Message, WebSocket};

pub(crate) const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HEADER: usize = 8192;
/// Maximum wire size of a single control frame (2-byte header + 125-byte
/// payload).  Reads of this size or smaller indicate that data arrived
/// slowly and count as "trickle" for flood detection.  Also used as the
/// divisor for message-completion flood relief.
const SMALL_READ_THRESHOLD: usize = 127;

// Frame header bits (RFC 6455 §5.2)
pub(crate) const FIN: u8 = 0b1000_0000;
pub(crate) const RESERVED_MASK: u8 = 0b0111_0000;
pub(crate) const OPCODE_MASK: u8 = 0b0000_1111;
pub(crate) const MASK_BIT: u8 = 0b1000_0000;
pub(crate) const LEN_MASK: u8 = 0b0111_1111;

// Opcodes (RFC 6455 §5.2)
pub(crate) const OP_CONTINUATION: u8 = 0x0;
pub(crate) const OP_TEXT: u8 = 0x1;
pub(crate) const OP_BINARY: u8 = 0x2;
pub(crate) const OP_CLOSE: u8 = 0x8;
pub(crate) const OP_PING: u8 = 0x9;
pub(crate) const OP_PONG: u8 = 0xA;

/// Close handshake state (RFC 6455 §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseState {
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
pub(crate) struct Buffers {
    /// Incoming byte buffer.  Non-fragmented messages are borrowed
    /// directly from here, avoiding copies.
    pub(crate) read: ReadBuf,
    /// Scratch space for line-end positions during header parsing.
    pub(crate) line_ends: Vec<usize>,
    /// Outgoing byte buffer; partially-flushed frames are retried on
    /// the next flush.
    pub(crate) send: SendBuf,
    /// Reassembly buffer for fragmented messages.  Each continuation
    /// frame's payload is appended here; the completed message borrows
    /// from this buffer.
    pub(crate) fragment_buf: Vec<u8>,
}

/// Protocol state that is never borrowed by a returned [`Message`].
pub(crate) struct Session {
    /// Opcode of the first frame in a fragmented message (0 = no active fragmentation).
    pub(crate) fragment_opcode: u8,
    pub(crate) close_state: CloseState,
    /// Maximum payload size (bytes) accepted for a single frame or
    /// reassembled fragmented message.
    pub(crate) max_payload: usize,
    /// Target buffer capacity. After processing a large message the read
    /// buffer is shrunk back to this size to avoid permanent memory bloat.
    pub(crate) max_buf_size: usize,
    /// Max frames processed per [`read_message`] call without producing
    /// a message before returning [`ReadStatus::Idle`].  `usize::MAX` = unlimited.
    pub(crate) frame_budget: usize,
    /// Running score for flood detection.  Incremented per frame,
    /// decremented per small read and per completed message.
    pub(crate) flood_score: usize,
    /// Threshold above which [`flood_score`] triggers an error.
    pub(crate) max_flood_score: usize,
    /// Comma-joined subprotocol tokens to send in `Sec-WebSocket-Protocol`.
    /// `None` means no subprotocol negotiation.
    pub(crate) subprotocols: Option<String>,
    /// The subprotocol selected by the server during the handshake.
    pub(crate) negotiated_subprotocol: Option<String>,
    /// Deadline by which a pong (or any frame) must arrive after a
    /// [`send_ping`](WebSocket::send_ping).  Cleared when any frame
    /// is received; triggers [`ConnError::PingTimeout`] if
    /// exceeded during an idle [`read_message`] call.
    pub(crate) ping_deadline: Option<Instant>,
    /// When the last frame was received.  Updated on every
    /// [`read_message`](WebSocket::read_message) call that processed
    /// at least one frame.
    pub(crate) last_activity: Option<Instant>,
}

impl Buffers {
    pub(crate) fn new() -> Self {
        Buffers {
            read: ReadBuf::with_capacity(MAX_HEADER),
            line_ends: Vec::with_capacity(16),
            send: SendBuf::with_capacity(256),
            fragment_buf: Vec::new(),
        }
    }

    pub(crate) fn read_message<S: Read + Write, R: Rng>(
        &mut self,
        stream: &mut S,
        sess: &mut Session,
        rng: &mut R,
    ) -> Result<Option<Message<'_>>, ConnError> {
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
                Err(e) => return Err(e),
            }
        }
        let mut budget = sess.frame_budget;
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
                && rng.one_in_eight_odds()
            {
                self.fragment_buf.clear();
                self.fragment_buf.shrink_to(sess.max_buf_size);
            }

            // Track how many bytes fill_from reads from the OS.
            let end_before = self.read.filled().len();

            // Parse frame header
            let (fin, opcode, payload_len) = {
                self.read.fill_from(stream, 2)?;
                let hdr = self.read.pending();
                let byte0 = hdr[0];
                let byte1 = hdr[1];

                let rsv = byte0 & RESERVED_MASK;
                if rsv != 0 {
                    return Err(ConnError::BadReservedBits(rsv));
                }

                let fin = byte0 & FIN != 0;
                let opcode = byte0 & OPCODE_MASK;
                let masked = byte1 & MASK_BIT != 0;
                if masked {
                    return Err(ConnError::MaskedServerFrame);
                }
                let len_byte = (byte1 & LEN_MASK) as usize;

                let (header_size, payload_len) = match len_byte {
                    n @ 0..=125 => (2, n),
                    126 => {
                        self.read.fill_from(stream, 4)?;
                        let hdr = self.read.pending();
                        let len = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
                        if len < 126 {
                            return Err(ConnError::NonMinimalLength);
                        }
                        if len > sess.max_payload {
                            return Err(ConnError::PayloadTooLarge(len as u64));
                        }
                        (4, len)
                    }
                    127 => {
                        self.read.fill_from(stream, 10)?;
                        let hdr = self.read.pending();
                        let len = u64::from_be_bytes(hdr[2..10].try_into().unwrap());
                        if len >> 63 != 0 {
                            return Err(ConnError::PayloadLengthMsb);
                        }
                        if len < 65536 {
                            return Err(ConnError::NonMinimalLength);
                        }
                        if len > sess.max_payload as u64 {
                            return Err(ConnError::PayloadTooLarge(len));
                        }
                        (10, len as usize)
                    }
                    _ => unreachable!("7-bit value is always 0..=127"),
                };

                if opcode >= OP_CLOSE {
                    if !fin {
                        return Err(ConnError::FragmentedControl);
                    }
                    if payload_len > 125 {
                        return Err(ConnError::ControlPayloadTooLarge(payload_len));
                    }
                }

                let frame_size = header_size + payload_len;
                self.read.fill_from(stream, frame_size)?;
                self.read.consume(frame_size);

                (fin, opcode, payload_len)
            };

            // A small read means data trickled in (one frame); a big
            // read (or no read at all) means data was already buffered.
            let bytes_read = self.read.filled().len() - end_before;
            if bytes_read > 0 && bytes_read <= SMALL_READ_THRESHOLD {
                sess.flood_score = sess.flood_score.saturating_sub(1);
            }

            // Flood detection: every frame adds pressure.
            sess.flood_score += 1;
            if sess.flood_score > sess.max_flood_score {
                return Err(ConnError::Flood);
            }

            let pos = self.read.pos();
            let payload_range = pos - payload_len..pos;

            match opcode {
                OP_CONTINUATION => {
                    if sess.fragment_opcode == 0 {
                        return Err(ConnError::UnexpectedContinuation);
                    }
                    let total = self.fragment_buf.len() + payload_len;
                    if total > sess.max_payload {
                        return Err(ConnError::FragmentedMessageTooLarge(total));
                    }
                    self.fragment_buf
                        .extend_from_slice(&self.read.filled()[payload_range]);
                    if fin {
                        let opcode = sess.fragment_opcode;
                        sess.fragment_opcode = 0;
                        let relief = 1.max(self.fragment_buf.len() / SMALL_READ_THRESHOLD);
                        sess.flood_score = sess.flood_score.saturating_sub(relief);
                        return into_message(opcode, &self.fragment_buf).map(Some);
                    }
                }

                OP_TEXT | OP_BINARY => {
                    if sess.fragment_opcode != 0 {
                        return Err(ConnError::DataDuringFragmentation);
                    }
                    if fin {
                        let relief = 1.max(payload_len / SMALL_READ_THRESHOLD);
                        sess.flood_score = sess.flood_score.saturating_sub(relief);
                        return into_message(opcode, &self.read.filled()[payload_range]).map(Some);
                    }
                    self.fragment_buf.clear();
                    self.fragment_buf
                        .extend_from_slice(&self.read.filled()[payload_range]);
                    sess.fragment_opcode = opcode;
                }

                OP_CLOSE | OP_PING | OP_PONG => {
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
                            // Pong replies are sent even during CloseSent:
                            // control frames are not data frames, and most
                            // peers expect pong responses until the TCP
                            // connection is torn down.
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
                        }
                    }
                }

                _ => return Err(ConnError::UnknownOpcode(opcode)),
            }

            // Budget: yield to the caller after processing too many
            // frames without producing a message.
            budget = budget.saturating_sub(1);
            if budget == 0 {
                return Ok(None);
            }
        }
    }
}

impl Session {
    pub(crate) fn new() -> Self {
        Session {
            fragment_opcode: 0,
            close_state: CloseState::Open,
            max_payload: crate::DEFAULT_MAX_PAYLOAD,
            max_buf_size: crate::DEFAULT_MAX_BUF_SIZE,
            frame_budget: usize::MAX,
            flood_score: 0,
            max_flood_score: 1000,
            subprotocols: None,
            negotiated_subprotocol: None,
            ping_deadline: None,
            last_activity: None,
        }
    }
}

impl<S: Read + Write, R: Rng> WebSocket<S, R> {
    /// Run the WebSocket opening handshake on the underlying stream.
    pub(crate) fn handshake(
        &mut self,
        host: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<(), ConnError> {
        self.bufs.read.clear();
        self.bufs.send.clear();
        self.bufs.line_ends.clear();
        self.bufs.fragment_buf.clear();
        self.sess.fragment_opcode = 0;
        self.sess.close_state = CloseState::Open;
        self.sess.flood_score = 0;
        self.sess.negotiated_subprotocol = None;
        self.sess.ping_deadline = None;
        self.sess.last_activity = None;

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
                 Sec-WebSocket-Version: 13\r\n"
            )
            .expect("write to Vec cannot fail");
            if let Some(ref protos) = self.sess.subprotocols {
                write!(buf, "Sec-WebSocket-Protocol: {protos}\r\n")
                    .expect("write to Vec cannot fail");
            }
            for &(name, value) in headers {
                write!(buf, "{name}: {value}\r\n").expect("write to Vec cannot fail");
            }
            buf.extend_from_slice(b"\r\n");
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
                .read_until::<_, ConnError>(&mut self.stream, MAX_HEADER, |buf| {
                    let data = buf.pending();
                    let limit = data.len().min(MAX_HEADER);
                    while let Some(off) = data[scan..limit].iter().position(|&b| b == b'\n') {
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
                    if data.len() >= MAX_HEADER {
                        return Err(ConnError::HeadersTooLarge);
                    }
                    Ok(None)
                })?
        };

        {
            let status = b"HTTP/1.1 101";
            let data = self.bufs.read.pending();
            if !(data[..header_end].starts_with(status)
                && matches!(data[status.len()], b' ' | b'\r' | b'\n'))
            {
                return Err(ConnError::BadStatus);
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
            let mut subprotocol = None;
            let mut line_start = 0;
            let data = self.bufs.read.pending();
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
                    } else if name.eq_ignore_ascii_case(b"sec-websocket-protocol") {
                        subprotocol = subprotocol.or(Some(value));
                    }
                }
                line_start = nl + 1;
            }

            let accept = accept.ok_or(ConnError::MissingAccept)?;
            if accept != expected {
                return Err(ConnError::BadAccept);
            }
            if !has_upgrade {
                return Err(ConnError::MissingUpgrade);
            }
            if !has_connection {
                return Err(ConnError::MissingConnection);
            }

            // RFC 6455 §4.2.2: if the server sends Sec-WebSocket-Protocol,
            // it must be one of the values the client offered.
            if let Some(selected) = subprotocol {
                let selected = std::str::from_utf8(selected)?;
                match self.sess.subprotocols {
                    Some(ref offered) => {
                        if !offered.split(',').any(|t| t.trim() == selected) {
                            return Err(ConnError::InvalidSubprotocol);
                        }
                        self.sess.negotiated_subprotocol = Some(selected.to_owned());
                    }
                    None => return Err(ConnError::InvalidSubprotocol),
                }
            }
        }

        self.bufs.read.consume(header_end);
        self.bufs.read.compact();
        self.bufs.line_ends.clear();
        self.bufs.line_ends.shrink_to(32);

        Ok(())
    }
}

/// Append a masked WebSocket frame to the send buffer.
pub(crate) fn build_frame(send: &mut SendBuf, rng: &mut impl Rng, opcode: u8, payload: &[u8]) {
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
    let mask = rng.next_u32().to_ne_bytes();
    send.push(&mask);
    send.push(payload);

    apply_mask(send.last_mut(payload.len()), mask);
}

/// XOR `buf` with `mask` repeated.  Aligns to a 16-byte boundary,
/// then uses u128-wide XOR for the bulk.
pub(crate) fn apply_mask(buf: &mut [u8], mask: [u8; 4]) {
    // Split off unaligned head bytes.
    let align = buf.as_ptr().align_offset(align_of::<u128>()).min(buf.len());
    let (head, rest) = buf.split_at_mut(align);
    for (i, b) in head.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }

    // Build the wide mask, rotated by the head length so mask bytes
    // stay in sync across the split.
    let rot = (align % 4) as u32 * 8;
    let m = u32::from_ne_bytes(mask);
    #[cfg(target_endian = "little")]
    let rotated = m.rotate_right(rot);
    #[cfg(target_endian = "big")]
    let rotated = m.rotate_left(rot);
    let r = rotated as u128;
    let mask128 = r | (r << 32) | (r << 64) | (r << 96);

    let (chunks, tail) = rest.as_chunks_mut::<16>();
    for chunk in chunks {
        *chunk = (u128::from_ne_bytes(*chunk) ^ mask128).to_ne_bytes();
    }
    let rotated_bytes = rotated.to_ne_bytes();
    for (i, b) in tail.iter_mut().enumerate() {
        *b ^= rotated_bytes[i % 4];
    }
}

pub(crate) fn validate_send_close_code(code: u16) -> Result<(), CallerError> {
    match code {
        1000..=1003 | 1007..=1014 | 3000..=4999 => Ok(()),
        _ => Err(CallerError::InvalidCloseCode(code)),
    }
}

fn validate_recv_close_code(code: u16) -> Result<(), ConnError> {
    match code {
        1005 | 1006 | 1015 => Err(ConnError::InvalidCloseCode(code)),
        1000..=4999 => Ok(()),
        _ => Err(ConnError::InvalidCloseCode(code)),
    }
}

fn into_message<'a>(opcode: u8, buf: &'a [u8]) -> Result<Message<'a>, ConnError> {
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
) -> Result<Message<'a>, ConnError> {
    if payload.len() == 1 {
        return Err(ConnError::BadClosePayload);
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
