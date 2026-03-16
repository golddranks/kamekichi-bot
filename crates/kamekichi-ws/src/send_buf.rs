use std::io::{self, Write};

use rand_core::Rng;

use crate::ConnectionError;

/// Outgoing byte buffer with a drain cursor.
///
/// Data is appended via [`push`](Self::push), then drained to a stream
/// via [`flush`](Self::flush).  Partially-written data is tracked by an
/// internal cursor and retried on the next flush.
pub(crate) struct SendBuf {
    buf: Vec<u8>,
    /// Bytes already written to the stream.  When `pos < buf.len()`
    /// a partial write is pending.
    pos: usize,
}

impl SendBuf {
    pub fn with_capacity(capacity: usize) -> Self {
        SendBuf {
            buf: Vec::with_capacity(capacity),
            pos: 0,
        }
    }

    /// Whether there is unsent data in the buffer.
    pub fn has_pending(&self) -> bool {
        self.pos < self.buf.len()
    }

    /// Number of bytes waiting to be sent.
    pub fn pending_len(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Append bytes to the buffer without sending.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Append a single byte.
    pub fn push_byte(&mut self, b: u8) {
        self.buf.push(b);
    }

    /// Reserve space for at least `additional` more bytes.
    pub fn reserve(&mut self, additional: usize) {
        self.buf.reserve(additional);
    }

    /// Mutable reference to the last `len` bytes in the buffer.
    pub fn last_mut(&mut self, len: usize) -> &mut [u8] {
        let start = self.buf.len() - len;
        &mut self.buf[start..]
    }

    /// Drain pending data to `stream`.  Retries on `Interrupted`;
    /// propagates all other errors.
    pub fn flush(&mut self, stream: &mut impl Write) -> Result<(), ConnectionError> {
        while self.pos < self.buf.len() {
            match stream.write(&self.buf[self.pos..]) {
                Ok(0) => return Err(ConnectionError::Closed),
                Ok(n) => self.pos += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Best-effort drain + stream flush.  Errors are silently ignored.
    pub fn try_flush(&mut self, stream: &mut impl Write) {
        let _ = self.flush(stream);
        let _ = stream.flush();
    }

    /// Probabilistically shrink the backing allocation if capacity
    /// exceeds `max_cap`.  Only shrinks when all data has been sent.
    pub fn maybe_shrink(&mut self, max_cap: usize, rng: &mut impl Rng) {
        if self.pos >= self.buf.len()
            && self.buf.capacity() > max_cap
            && rng.next_u32() & 0b111 == 0
        {
            self.buf.clear();
            self.pos = 0;
            self.buf.shrink_to(max_cap);
        }
    }

    /// Clear and return a mutable reference to the backing buffer
    /// for use as scratch space.
    pub fn as_scratch(&mut self) -> &mut Vec<u8> {
        self.buf.clear();
        self.pos = 0;
        &mut self.buf
    }

    /// Discard all data.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.pos = 0;
    }
}
