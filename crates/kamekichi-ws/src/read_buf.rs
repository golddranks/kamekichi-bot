use std::io::{self, Read};
use std::ops::Range;

use rand_core::Rng;

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

    /// Read from `reader` in a loop, calling `f` after each read.
    ///
    /// If the buffer already contains data, `f` is called once before
    /// the first read so that pre-existing data is not overlooked.
    ///
    /// `f` receives `&Self` and should return:
    /// - `Ok(Some(value))` — stop and return the value
    /// - `Ok(None)` — keep reading
    /// - `Err(e)` — abort with an error
    ///
    /// `max_len` limits the total buffer size; once reached, no further
    /// reads are attempted and `f` gets a final call to inspect the
    /// full buffer and decide whether to return a value or an error.
    pub fn read_until<T, E>(
        &mut self,
        reader: &mut impl Read,
        max_len: usize,
        mut f: impl FnMut(&Self) -> Result<Option<T>, E>,
    ) -> Result<T, E>
    where
        E: From<FillError>,
    {
        if self.buf.len() < max_len {
            self.buf.resize(max_len, 0);
        }
        // Check pre-existing data before the first (potentially blocking) read.
        if self.end > self.start
            && let Some(v) = f(self)?
        {
            return Ok(v);
        }
        loop {
            if self.end >= self.buf.len() {
                // Buffer is full — f already saw all data on the previous
                // iteration.  No room for more reads.
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

    /// The valid portion of the backing buffer (`[0..end]`).
    pub fn as_slice(&self) -> &[u8] {
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
        if self.start == 0 && self.buf.capacity() > max_cap && rng.next_u32() & 0b111 == 0 {
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
