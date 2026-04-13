// Without this cfg, some versions tarpaulin consider this module as
// a normal code module and report coverage for it.
#![cfg(test)]

use std::io::{self, Cursor};

use super::*;
use crate::Rng;
use read_buf::FillError;

struct CounterRng(u32);

impl Rng for CounterRng {
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let bytes = self.next_u32().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    fn next_u32(&mut self) -> u32 {
        let v = self.0;
        self.0 = self.0.wrapping_add(1);
        v
    }
}

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
    subprotocol: Option<String>,
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
            subprotocol: None,
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
            subprotocol: None,
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
            subprotocol: None,
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
            subprotocol: None,
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
            subprotocol: None,
        }
    }
    fn auto_with_subprotocol(proto: &str) -> Self {
        ConnectMock {
            subprotocol: Some(proto.to_owned()),
            ..ConnectMock::auto()
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
            subprotocol: None,
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
            if let Some(ref proto) = self.subprotocol {
                response.push_str(&format!("Sec-WebSocket-Protocol: {proto}{nl}"));
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
        WebSocket::new(CounterRng(0)).connect(ConnectMock::with_response(rx), "example.com", "/"),
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
        WebSocket::new(CounterRng(0)).connect(ConnectMock::with_response(rx), "example.com", "/"),
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
        WebSocket::new(CounterRng(0)).connect(ConnectMock::with_response(rx), "example.com", "/"),
        Err(Error::Reconnect(ConnectionError::BadAccept))
    ));
}

#[test]
fn connect_headers_too_large() {
    let rx = vec![b'X'; 8300];
    assert!(matches!(
        WebSocket::new(CounterRng(0)).connect(ConnectMock::with_response(rx), "example.com", "/"),
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
        WebSocket::new(CounterRng(0)).connect(ConnectMock::with_response(rx), "example.com", "/"),
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
    let mut ws =
        WebSocket::new(CounterRng(0)).with_stream(ChunkedMock::with_blocks(data, usize::MAX, 1));
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
fn flood_detected() {
    // All pings pre-buffered → large initial read, then bytes_read=0
    // for subsequent frames → no score decrement → score climbs.
    let mut data = Vec::new();
    for _ in 0..5 {
        data.extend(frame(true, OP_PING, b""));
    }
    let mut ws = WebSocket::new(CounterRng(0))
        .max_flood_score(3)
        .with_stream(MockStream::new(data));

    assert!(matches!(
        ws.read_message(),
        Err(Error::Reconnect(ConnectionError::Flood))
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
fn frame_budget_returns_none() {
    let mut data = Vec::new();
    for _ in 0..5 {
        data.extend(frame(true, OP_PING, b""));
    }
    data.extend(frame(true, OP_TEXT, b"after"));

    let mut ws = WebSocket::new(CounterRng(0))
        .frame_budget(3)
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
    let Err((err, _ws, _stream)) = ws.try_connect(ConnectMock::auto(), "example.com", "/bad\npath")
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
        ConnectionError::Flood,
        ConnectionError::InvalidSubprotocol,
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

impl Rng for ZeroRng {
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        dest.fill(0);
    }

    fn next_u32(&mut self) -> u32 {
        0
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

// ---- Subprotocol negotiation ----

#[test]
fn connect_with_subprotocol() {
    let ws = WebSocket::new(CounterRng(0))
        .subprotocols(&["chat", "superchat"])
        .connect(ConnectMock::auto_with_subprotocol("chat"), "example.com", "/ws")
        .unwrap();
    assert_eq!(ws.subprotocol(), Some("chat"));
    // Verify the request included the Sec-WebSocket-Protocol header.
    let req = &ws.inner().tx;
    assert!(req
        .windows(b"Sec-WebSocket-Protocol: chat, superchat".len())
        .any(|w| w == b"Sec-WebSocket-Protocol: chat, superchat"));
}

#[test]
fn connect_subprotocol_not_offered() {
    let result = WebSocket::new(CounterRng(0))
        .subprotocols(&["chat", "superchat"])
        .connect(ConnectMock::auto_with_subprotocol("nope"), "example.com", "/ws");
    assert!(matches!(
        result,
        Err(Error::Reconnect(ConnectionError::InvalidSubprotocol))
    ));
}

#[test]
fn connect_subprotocol_unrequested() {
    // Client doesn't set subprotocols, but server sends one.
    let result = WebSocket::new(CounterRng(0))
        .connect(ConnectMock::auto_with_subprotocol("chat"), "example.com", "/ws");
    assert!(matches!(
        result,
        Err(Error::Reconnect(ConnectionError::InvalidSubprotocol))
    ));
}

// ---- Custom headers ----

#[test]
fn connect_with_headers_success() {
    let ws = WebSocket::new(CounterRng(0))
        .connect_with_headers(
            ConnectMock::auto(),
            "example.com",
            "/ws",
            &[("Authorization", "Bearer tok")],
        )
        .unwrap();
    let req = &ws.inner().tx;
    assert!(req
        .windows(b"Authorization: Bearer tok".len())
        .any(|w| w == b"Authorization: Bearer tok"));
}

#[test]
fn try_connect_rejects_header_name_crlf() {
    let result = WebSocket::new(CounterRng(0)).try_connect_with_headers(
        ConnectMock::auto(),
        "example.com",
        "/ws",
        &[("Bad\r\nName", "value")],
    );
    assert!(matches!(
        result,
        Err((Error::Fatal(CallerError::InvalidHeaderValue), _, _))
    ));
}

#[test]
fn try_connect_rejects_header_value_crlf() {
    let result = WebSocket::new(CounterRng(0)).try_connect_with_headers(
        ConnectMock::auto(),
        "example.com",
        "/ws",
        &[("Name", "bad\r\nvalue")],
    );
    assert!(matches!(
        result,
        Err((Error::Fatal(CallerError::InvalidHeaderValue), _, _))
    ));
}

// ---- inner_mut ----

#[test]
fn inner_mut_accessible() {
    let mut ws = ws(frame(true, OP_TEXT, b"hi"));
    let _stream: &mut MockStream = ws.inner_mut();
}

// ---- Frame budget on continuation frames ----

#[test]
fn frame_budget_on_continuation_frames() {
    let mut data = Vec::new();
    data.extend(frame(false, OP_TEXT, b"1"));
    for i in 2..=6 {
        data.extend(frame(false, OP_CONTINUATION, &[b'0' + i]));
    }
    data.extend(frame(true, OP_CONTINUATION, b"7"));

    let mut ws = WebSocket::new(CounterRng(0))
        .frame_budget(3)
        .with_stream(MockStream::new(data));

    // First call: frames 1,2,3 → budget exhausted
    assert!(matches!(ws.read_message(), Ok(None)));
    // Second call: frames 4,5,6 → budget exhausted
    assert!(matches!(ws.read_message(), Ok(None)));
    // Third call: frame 7 (FIN) → completed message
    match ws.read_message().unwrap().unwrap() {
        Message::Text(t) => assert_eq!(t, "1234567"),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ---- Rng default next_u32 ----

#[test]
fn rng_default_next_u32() {
    struct FillOnly;
    impl Rng for FillOnly {
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for (i, b) in dest.iter_mut().enumerate() {
                *b = (i as u8).wrapping_add(1);
            }
        }
    }
    // Default next_u32 calls fill_bytes with a 4-byte buffer,
    // then interprets as little-endian.
    assert_eq!(Rng::next_u32(&mut FillOnly), u32::from_le_bytes([1, 2, 3, 4]));
}

// ---- Send + Sync static assertions ----

const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<Error>();
        _assert_sync::<Error>();
        _assert_send::<ConnectionError>();
        _assert_sync::<ConnectionError>();
        _assert_send::<CallerError>();
        _assert_sync::<CallerError>();
    }
};
