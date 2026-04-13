use std::io::{self, Read};
use std::ops::Range;

use crate::rng::Rng;

const MIN_READ_BUF: usize = 4096;

/// Error from [`ReadBuf::fill_from`].
#[derive(Debug)]
pub(crate) enum FillError {
    /// The stream reached EOF before enough data was available.
    Eof,
    /// An IO error occurred while reading.
    Io(io::Error),
    /// The buffer reached its size limit before the callback produced a value.
    BufferFull,
}

/// A byte buffer with separate read and write cursors.
///
/// Data flows in three regions:
///
/// ```text
///  [ consumed | pending ... | spare (zeroed) ... ]
///    0      start          end               buf.len()
/// ```
///
/// - `pending()` returns `&buf[start..end]` — data read but not yet processed.
/// - `buf[end..len]` is pre-zeroed space available for the next `read()`.
/// - After processing, call `consume(n)` to advance `start`.
/// - `compact()` shifts pending data to the front, reclaiming consumed space.
pub(crate) struct ReadBuf {
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl ReadBuf {
    pub fn with_capacity(capacity: usize) -> Self {
        ReadBuf {
            buf: Vec::with_capacity(capacity),
            start: 0,
            end: 0,
        }
    }

    // -- Query --

    /// Unconsumed data in the buffer.
    pub fn pending(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    /// Absolute offset of the read cursor into the backing buffer.
    /// Useful for computing payload ranges after `consume`.
    pub fn pos(&self) -> usize {
        self.start
    }

    // -- Consume --

    /// Advance the read cursor past `n` bytes of pending data.
    pub fn consume(&mut self, n: usize) {
        debug_assert!(self.start + n <= self.end);
        self.start += n;
    }

    // -- Fill --

    /// Ensure at least `need` bytes are available in [`pending()`](Self::pending),
    /// reading from `reader` as necessary.
    ///
    /// May read more than `need` bytes if the OS provides them — the extra
    /// data stays buffered for subsequent calls.
    pub fn fill_from(&mut self, reader: &mut impl Read, need: usize) -> Result<(), FillError> {
        let target = self.start + need;
        if self.end >= target {
            return Ok(());
        }
        // Grow once — zeroes only the new bytes.
        if self.buf.len() < target {
            self.buf.resize(target.max(self.end + MIN_READ_BUF), 0);
        }
        while self.end < target {
            match reader.read(&mut self.buf[self.end..]) {
                Ok(0) => return Err(FillError::Eof),
                Ok(n) => self.end += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(FillError::Io(e)),
            }
        }
        Ok(())
    }

    /// Read from `reader` in a loop, calling `f` after each read
    /// until `f` returns `Ok(Some(value))` or an error occurs.
    /// `f` may also return `Ok(None)` to request more data.
    ///
    /// If there is already unprocessed data in the buffer (`start <
    /// end`), `f` is called once before the first read.
    ///
    /// `max_len` bounds how long the search continues: once at least
    /// `max_len` bytes of unprocessed data have accumulated and `f`
    /// hasn't produced a value after the read that crossed the threshold,
    /// [`FillError::BufferFull`] is returned.  A single read may also buffer
    /// data beyond `max_len`; that extra data remains available in the buffer for
    /// later operations.
    pub fn read_until<T, E>(
        &mut self,
        reader: &mut impl Read,
        max_len: usize,
        mut f: impl FnMut(&Self) -> Result<Option<T>, E>,
    ) -> Result<T, E>
    where
        E: From<FillError>,
    {
        let limit = self.start + max_len;
        if self.buf.len() < limit {
            self.buf.resize(limit, 0);
        }
        // Check pre-existing data before the first (potentially blocking) read.
        if self.end > self.start
            && let Some(v) = f(self)?
        {
            return Ok(v);
        }
        loop {
            if self.end - self.start >= max_len {
                // Pending data reached the caller's limit — f already
                // saw all data on the previous iteration.
                return Err(FillError::BufferFull.into());
            }
            match reader.read(&mut self.buf[self.end..]) {
                Ok(0) => return Err(FillError::Eof.into()),
                Ok(n) => self.end += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(FillError::Io(e).into()),
            }
            if let Some(v) = f(self)? {
                return Ok(v);
            }
        }
    }

    // -- Raw access (for Payload construction) --

    /// Access the backing buffer by absolute range.
    pub fn get(&self, range: Range<usize>) -> &[u8] {
        &self.buf[range]
    }

    /// All data read into the buffer so far (`[0..end]`), including
    /// any bytes already advanced past by [`consume`](Self::consume).
    pub fn filled(&self) -> &[u8] {
        &self.buf[..self.end]
    }

    // -- Memory management --

    /// Shift unconsumed data to the front of the buffer.
    /// Preserves spare space beyond `end` so subsequent reads
    /// don't need to resize.
    pub fn compact(&mut self) {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        } else if self.start > 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
    }

    /// Compact when the consumed prefix wastes enough memory.
    /// Returns `true` if compaction actually happened.
    pub fn maybe_compact(&mut self, threshold: usize) -> bool {
        if self.start == self.end || (self.start > threshold && self.start > self.end / 2) {
            self.compact();
            true
        } else {
            false
        }
    }

    /// Probabilistically shrink the backing allocation if capacity
    /// exceeds `max_cap`.  Only effective after a compact (`start == 0`).
    pub fn maybe_shrink_capacity(&mut self, max_cap: usize, rng: &mut impl Rng) {
        if self.start == 0 && self.buf.capacity() > max_cap && rng.one_in_eight_odds() {
            self.buf.truncate(self.end);
            self.buf.shrink_to(max_cap);
        }
    }

    /// Discard all data, keeping the allocation.
    pub fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }
}
