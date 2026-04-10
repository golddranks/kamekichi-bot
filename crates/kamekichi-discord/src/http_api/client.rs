use std::cell::RefCell;
use std::fmt::Display;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::ops::Not;
use std::sync::Arc;
use std::time::Duration;

use crate::{Error, TlsStream};

// TODOs
// constify the HTTP status codes

const MAX_RESPONSE_BODY: usize = 16 * 1024 * 1024; // 16 MiB

pub struct Client {
    pub(crate) host: String,
    pub(crate) token: String,
    pub(crate) user_agent: String,
    pub(crate) tls_config: Arc<rustls::ClientConfig>,
    pub(crate) conn: RefCell<Option<TlsStream>>,
}

#[derive(Clone, Copy)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl Method {
    fn is_idempotent(&self) -> bool {
        matches!(self, Method::Get | Method::Delete | Method::Patch)
    }
}

impl Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Method::Get => write!(f, "GET"),
            Method::Post => write!(f, "POST"),
            Method::Put => write!(f, "PUT"),
            Method::Delete => write!(f, "DELETE"),
            Method::Patch => write!(f, "PATCH"),
        }
    }
}

impl Client {
    pub fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), Error> {
        const MAX_ATTEMPTS: u32 = 4;
        let mut last_status = 0;
        for attempt in 0..MAX_ATTEMPTS {
            let (status, retry_after, resp_body) = self.request_once(method, path, body)?;
            if status == 429 {
                last_status = status;
                if attempt + 1 < MAX_ATTEMPTS {
                    let wait = retry_after.unwrap_or(1.0).clamp(0.0, 60.0);
                    std::thread::sleep(Duration::from_secs_f64(wait));
                }
                continue;
            }
            if status >= 500 {
                last_status = status;
                if attempt + 1 < MAX_ATTEMPTS {
                    let wait = (1u64 << attempt).min(8) as f64;
                    std::thread::sleep(Duration::from_secs_f64(wait));
                }
                continue;
            }
            if status >= 400 {
                eprintln!("Discord API error {status}: {method} {path}\n{resp_body}");
            }
            return Ok((status, resp_body));
        }
        Err(Error::RetriesExhausted {
            status: last_status,
        })
    }

    fn connect(&self) -> Result<TlsStream, Error> {
        let server_name = rustls::pki_types::ServerName::try_from(self.host.as_str())?.to_owned();
        let tls_conn = rustls::ClientConnection::new(self.tls_config.clone(), server_name)?;
        let tcp = TcpStream::connect((self.host.as_str(), 443))?;
        tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
        Ok(rustls::StreamOwned::new(tls_conn, tcp))
    }

    fn request_once(
        &self,
        method: Method,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, Option<f64>, String), Error> {
        let mut conn = self.conn.borrow_mut();
        let mut stream = match conn.take() {
            Some(s) => s,
            None => self.connect()?,
        };

        match send_and_read(
            &mut stream,
            &self.token,
            &self.user_agent,
            method,
            path,
            body,
        ) {
            Ok((status, retry_after, resp_body, keep_alive)) => {
                if keep_alive {
                    *conn = Some(stream);
                }
                Ok((status, retry_after, resp_body))
            }
            Err(e) => {
                // Stale connection — retry once with a fresh one, but only
                // for idempotent methods.  For POST the server may have
                // already processed the request before the connection
                // dropped, so retrying could create duplicates.
                drop(stream);
                if method.is_idempotent().not() {
                    return Err(e);
                }
                eprintln!("Stale connection ({e}), retrying with fresh connection");
                let mut stream = self.connect()?;
                let (status, retry_after, resp_body, keep_alive) = send_and_read(
                    &mut stream,
                    &self.token,
                    &self.user_agent,
                    method,
                    path,
                    body,
                )?;
                if keep_alive {
                    *conn = Some(stream);
                }
                Ok((status, retry_after, resp_body))
            }
        }
    }
}

fn send_and_read(
    stream: &mut (impl Read + Write),
    token: &str,
    user_agent: &str,
    method: Method,
    path: &str,
    body: Option<&str>,
) -> Result<(u16, Option<f64>, String, bool), Error> {
    let host = "discord.com";

    write!(
        stream,
        "{method} {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Authorization: Bot {token}\r\n\
         User-Agent: {user_agent}\r\n",
    )?;
    if let Some(body) = body {
        write!(
            stream,
            "Content-Type: application/json\r\n\
             Content-Length: {len}\r\n\
             \r\n",
            len = body.len(),
        )?;
        stream.write_all(body.as_bytes())?;
    } else {
        write!(stream, "\r\n")?;
    }
    stream.flush()?;

    // Read headers
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 16384 {
            return Err(Error::HeadersTooLarge);
        }
    };

    let headers = &buf[..header_end];

    // Parse status line from bytes
    let first_line_end = headers
        .iter()
        .position(|&b| b == b'\r')
        .unwrap_or(header_end);
    let status_line = &headers[..first_line_end];
    let status = status_line
        .split(|&b| b == b' ')
        .nth(1)
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(Error::BadStatus)?;

    // Parse headers on byte slices — no allocation per line.
    let mut retry_after: Option<f64> = None;
    let mut keep_alive = true;
    let mut chunked = false;
    let mut content_length: Option<usize> = None;

    let mut pos = first_line_end + 2; // skip past first line's \r\n
    while pos < header_end {
        let line_end = headers[pos..]
            .iter()
            .position(|&b| b == b'\r')
            .map_or(header_end, |p| pos + p);
        let line = &headers[pos..line_end];
        pos = line_end + 2; // skip \r\n

        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = &line[..colon];
        let val = line[colon + 1..].trim_ascii();

        if retry_after.is_none()
            && (name.eq_ignore_ascii_case(b"retry-after")
                || name.eq_ignore_ascii_case(b"x-ratelimit-reset-after"))
        {
            retry_after = std::str::from_utf8(val)
                .ok()
                .and_then(|s| s.parse::<f64>().ok());
        } else if name.eq_ignore_ascii_case(b"connection") && val.eq_ignore_ascii_case(b"close") {
            keep_alive = false;
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            chunked = val.windows(7).any(|w| w.eq_ignore_ascii_case(b"chunked"));
        } else if name.eq_ignore_ascii_case(b"content-length") {
            content_length = std::str::from_utf8(val).ok().and_then(|s| s.parse().ok());
        }
    }

    // Body bytes already buffered past the header delimiter
    let body_start = header_end + 4;
    let leftover = buf[body_start..].to_vec();

    let resp_body = if chunked {
        read_chunked_body(stream, leftover)?
    } else if let Some(len) = content_length {
        read_fixed_body(stream, leftover, len)?
    } else if status == 204 || status == 304 {
        Vec::new()
    } else {
        // No framing info — read to end (connection not reusable)
        let mut body = leftover;
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
            if body.len() > MAX_RESPONSE_BODY {
                return Err(Error::ResponseTooLarge(body.len()));
            }
        }
        let resp = String::from_utf8(body).map_err(|_| Error::InvalidUtf8)?;
        return Ok((status, retry_after, resp, false));
    };

    let resp = String::from_utf8(resp_body).map_err(|_| Error::InvalidUtf8)?;
    Ok((status, retry_after, resp, keep_alive))
}

fn read_fixed_body(
    stream: &mut impl Read,
    leftover: Vec<u8>,
    content_length: usize,
) -> Result<Vec<u8>, Error> {
    if content_length > MAX_RESPONSE_BODY {
        return Err(Error::ResponseTooLarge(content_length));
    }
    let have = leftover.len();
    let mut body = leftover;
    if have < content_length {
        body.resize(content_length, 0);
        stream.read_exact(&mut body[have..])?;
    } else {
        body.truncate(content_length);
    }
    Ok(body)
}

fn read_chunked_body(stream: &mut impl Read, leftover: Vec<u8>) -> Result<Vec<u8>, Error> {
    let mut buf = leftover;
    let mut pos = 0;
    let mut result = Vec::new();

    loop {
        // Read chunk size line
        let crlf = loop {
            if let Some(p) = buf[pos..].windows(2).position(|w| w == b"\r\n") {
                break pos + p;
            }
            let mut tmp = [0u8; 4096];
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return Err(Error::ConnectionClosed);
            }
            buf.extend_from_slice(&tmp[..n]);
        };

        let size_str = std::str::from_utf8(&buf[pos..crlf]).map_err(|_| Error::MalformedChunk)?;
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| Error::MalformedChunk)?;

        pos = crlf + 2;

        if size == 0 {
            // Consume trailer headers + final CRLF (RFC 7230 §4.1)
            loop {
                let crlf2 = loop {
                    if let Some(p) = buf[pos..].windows(2).position(|w| w == b"\r\n") {
                        break pos + p;
                    }
                    let mut tmp = [0u8; 4096];
                    let n = stream.read(&mut tmp)?;
                    if n == 0 {
                        return Ok(result);
                    }
                    buf.extend_from_slice(&tmp[..n]);
                };
                if crlf2 == pos {
                    break;
                }
                pos = crlf2 + 2;
            }
            break;
        }

        if size > MAX_RESPONSE_BODY {
            return Err(Error::ResponseTooLarge(size));
        }
        if result.len() + size > MAX_RESPONSE_BODY {
            return Err(Error::ResponseTooLarge(result.len() + size));
        }

        // Ensure we have size + 2 bytes (chunk data + trailing CRLF)
        let need = size.checked_add(2).ok_or(Error::MalformedChunk)?;
        while buf.len() - pos < need {
            let mut tmp = [0u8; 4096];
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return Err(Error::ConnectionClosed);
            }
            buf.extend_from_slice(&tmp[..n]);
        }

        result.extend_from_slice(&buf[pos..pos + size]);
        pos += need;

        if pos > 8192 {
            buf.copy_within(pos.., 0);
            buf.truncate(buf.len() - pos);
            pos = 0;
        }
    }

    Ok(result)
}

pub fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::new();
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'@' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0F) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.rx.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn http_response(status: u16, headers: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} OK\r\n\
             Content-Length: {len}\r\n\
             {headers}\
             \r\n\
             {body}",
            len = body.len(),
        )
        .into_bytes()
    }

    fn do_request(response: Vec<u8>) -> Result<(u16, Option<f64>, String, bool), Error> {
        let mut stream = MockStream::new(response);
        send_and_read(&mut stream, "tok", "test-agent", Method::Get, "/test", None)
    }

    // -- Fixed body --

    #[test]
    fn fixed_body_happy_path() {
        let resp = http_response(200, "", r#"{"ok":true}"#);
        let (status, _, body, keep_alive) = do_request(resp).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, r#"{"ok":true}"#);
        assert!(keep_alive);
    }

    #[test]
    fn fixed_body_too_large() {
        let len = MAX_RESPONSE_BODY + 1;
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n").into_bytes();
        assert!(matches!(do_request(resp), Err(Error::ResponseTooLarge(_))));
    }

    // -- Chunked body --

    #[test]
    fn chunked_body_happy_path() {
        let resp = b"HTTP/1.1 200 OK\r\n\
                     Transfer-Encoding: chunked\r\n\
                     \r\n\
                     5\r\nhello\r\n\
                     6\r\n world\r\n\
                     0\r\n\r\n"
            .to_vec();
        let (status, _, body, _) = do_request(resp).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "hello world");
    }

    #[test]
    fn chunked_body_single_chunk_too_large() {
        let size = MAX_RESPONSE_BODY + 1;
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             {size:x}\r\n"
        )
        .into_bytes();
        assert!(matches!(do_request(resp), Err(Error::ResponseTooLarge(_))));
    }

    // -- Header case insensitivity --

    #[test]
    fn header_case_insensitive() {
        let resp = b"HTTP/1.1 200 OK\r\n\
                     CONTENT-LENGTH: 2\r\n\
                     \r\n\
                     ok"
        .to_vec();
        let (status, _, body, _) = do_request(resp).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "ok");
    }

    // -- Connection: close --

    #[test]
    fn connection_close_header() {
        let resp = http_response(200, "Connection: close\r\n", "ok");
        let (_, _, _, keep_alive) = do_request(resp).unwrap();
        assert!(!keep_alive);
    }

    // -- 204 No Content --

    #[test]
    fn no_content_204() {
        let resp = b"HTTP/1.1 204 No Content\r\n\r\n".to_vec();
        let (status, _, body, _) = do_request(resp).unwrap();
        assert_eq!(status, 204);
        assert!(body.is_empty());
    }

    // -- Invalid UTF-8 --

    #[test]
    fn invalid_utf8_body_rejected() {
        let invalid = [0xFF, 0xFE];
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
            .into_bytes()
            .into_iter()
            .chain(invalid)
            .collect::<Vec<_>>();
        assert!(matches!(do_request(resp), Err(Error::InvalidUtf8)));
    }

    // -- Retry-After --

    #[test]
    fn retry_after_parsed() {
        let resp = http_response(429, "Retry-After: 1.5\r\n", "{}");
        let (status, retry_after, _, _) = do_request(resp).unwrap();
        assert_eq!(status, 429);
        assert_eq!(retry_after, Some(1.5));
    }

    // -- Malformed status --

    #[test]
    fn malformed_status_line() {
        let resp = b"GARBAGE\r\n\r\n".to_vec();
        assert!(matches!(do_request(resp), Err(Error::BadStatus)));
    }
}
