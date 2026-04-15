//! Single-allocation read buffer for streaming protocol parsing.
//!
//! # Layout
//!
//! ```text
//!  [ consumed | pending | initialized | allocated ]
//!  0        start      end           len         cap
//! ```
//!
//! - **consumed** (`0..start`) — data already processed by the caller. Still
//!   accessible via `ReadBuf::all_read` / `ReadBuf::slice` with absolute
//!   ranges until the next `ReadBuf::compact`.
//! - **pending** (`start..end`) — data received but not yet processed.
//! - **initialized** (`end..len`) — initialized memory available for the
//!   next `Read::read` call. `len` only shrinks via an explicit
//!   [`ReadBuf::maybe_shrink_capacity`] call, so zero-initializing
//!   the same region again and again is avoided.
//! - **allocated** (`len..cap`) — allocated but uninitialized; can be
//!   initialized without reallocating.
//!
//! # Filling
//!
//! Two fill strategies:
//!
//! - [`ReadBuf::fill_from`] — for known lengths: block until at least `need`
//!   pending bytes are available. Used for WebSocket frames
//!   where the payload length is known upfront.
//!
//! - [`ReadBuf::read_until`] — search-driven: read in a loop, invoking a
//!   callback after each read until it produces a value. Used for HTTP
//!   response header parsing where the terminator position is unknown.
//!
//! Both retry immediately on `EINTR`, treat `read() → 0` as EOF,
//! and return `WouldBlock` when the stream read would block or time out,
//! to allow the caller to retry the operation later.
//!
//! # Consumed-but-accessible data
//!
//! [`consume`](ReadBuf::consume) advances `start` but does **not** discard
//! the bytes — they remain in `buf[0..start]` until the next
//! [`compact`](ReadBuf::compact). This allows callers to consume a
//! frame header while still borrowing the payload via
//! [`ReadBuf::cursor`], [`ReadBuf::all_read`], or [`ReadBuf::slice`] with
//! absolute ranges — without copying to a separate buffer.
//!
//! # Memory lifecycle
//!
//! The buffer grows as needed during fills but never shrinks on its
//! own. The caller is responsible for calling
//! [`ReadBuf::maybe_compact`] and [`ReadBuf::maybe_shrink_capacity`]
//! to reclaim memory.

use std::io::{self, Read};
use std::ops::Range;

use crate::rng::Rng;

/// Floor for the readable slice passed to `Read::read`, to avoid tiny reads.
const MIN_READ_BUF: usize = 4096;

/// Extra headroom past the requested length, so reads near the target still
/// get reasonably sized slices.
const MIN_OVERSHOOT: usize = 512;

/// Error from [`ReadBuf::read_until`].
#[derive(Debug)]
pub(crate) enum ReadUntilError<E> {
    /// The stream reached EOF before enough data was available.
    Eof,
    /// An IO error occurred while reading.
    Io(io::Error),
    /// The stream returned `WouldBlock` or `TimedOut`.
    WouldBlock,
    /// Pending data reached the caller's `max_len` limit before the callback produced a value.
    LimitReached,
    /// The user's callback's error for passing through.
    UserError(E),
}

/// Error from [`ReadBuf::fill_from`].
#[derive(Debug)]
pub(crate) enum FillError {
    /// The stream reached EOF before enough data was available.
    Eof,
    /// An IO error occurred while reading.
    Io(io::Error),
    /// The stream returned `WouldBlock` or `TimedOut`.
    WouldBlock,
}

impl FillError {
    fn into_read_until<E>(self) -> ReadUntilError<E> {
        match self {
            FillError::Eof => ReadUntilError::Eof,
            FillError::WouldBlock => ReadUntilError::WouldBlock,
            FillError::Io(e) => ReadUntilError::Io(e),
        }
    }
}

impl<E> From<E> for ReadUntilError<E> {
    fn from(e: E) -> Self {
        ReadUntilError::UserError(e)
    }
}

/// The main buffer container. See [module docs](self) for layout and design.
pub(crate) struct ReadBuf {
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl ReadBuf {
    /// Create an empty buffer with the given initial allocation.
    pub fn with_capacity(capacity: usize) -> Self {
        ReadBuf {
            buf: Vec::with_capacity(capacity),
            start: 0,
            end: 0,
        }
    }

    /// Get unconsumed data in the buffer.
    pub fn pending(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    /// Access the backing buffer by absolute range.
    pub fn slice(&self, range: Range<usize>) -> &[u8] {
        &self.buf[range]
    }

    /// All data read into the buffer so far (`[0..end]`), including
    /// any bytes already advanced past by [`consume`](Self::consume).
    pub fn all_read(&self) -> &[u8] {
        &self.buf[..self.end]
    }

    /// Absolute offset of the consume cursor into the backing buffer.
    pub fn cursor(&self) -> usize {
        self.start
    }

    /// Advance the read cursor past `n` bytes of pending data.
    pub fn consume(&mut self, n: usize) {
        assert!(self.start + n <= self.end);
        self.start += n;
    }

    /// Ensure at least `need` bytes are available in [`pending()`](Self::pending),
    /// reading from `reader` as necessary.
    ///
    /// May read more than `need` bytes if the OS provides them — the extra
    /// data stays buffered for subsequent calls.
    pub fn fill_from(&mut self, reader: &mut impl Read, need: usize) -> Result<(), FillError> {
        let target = self.start.saturating_add(need);
        if self.end >= target {
            return Ok(());
        }
        self.ensure_initialized(need);
        while self.end < target {
            self.read_once(reader)?;
        }
        Ok(())
    }

    /// Read from `reader` in a loop, calling `f` after each read
    /// until `f` returns `Ok(Some(value))` or an error occurs.
    /// `f` may return `Ok(None)` to request more data.
    ///
    /// If there is already pending data in the buffer (`start <
    /// end`), `f` is called once before the first read.
    ///
    /// `limit` bounds how long the search continues: once at least
    /// `limit` bytes of pending data have accumulated and `f`
    /// hasn't produced a value, [`ReadUntilError::LimitReached`] is
    /// returned. A single read may buffer data beyond `limit`;
    /// that extra data remains available for later operations.
    pub fn read_until<T, E>(
        &mut self,
        reader: &mut impl Read,
        limit: usize,
        mut f: impl FnMut(&Self) -> Result<Option<T>, E>,
    ) -> Result<T, ReadUntilError<E>> {
        // Check pre-existing data before the first (potentially blocking) read.
        if self.end > self.start
            && let Some(v) = f(self)?
        {
            return Ok(v);
        }
        self.ensure_initialized(limit);
        loop {
            if self.end - self.start >= limit {
                // Pending data reached the caller's limit — f already
                // saw all data on the previous iteration.
                return Err(ReadUntilError::LimitReached);
            }
            self.read_once(reader).map_err(FillError::into_read_until)?;
            if let Some(v) = f(self)? {
                return Ok(v);
            }
        }
    }

    /// Ensure at least `min_len` bytes past `end` are initialized.
    /// May allocate more than requested for efficiency.
    fn ensure_initialized(&mut self, min_len: usize) {
        let needed = self
            .end
            .saturating_add(min_len.saturating_add(MIN_OVERSHOOT).max(MIN_READ_BUF));
        if self.buf.len() < needed {
            self.buf.resize(needed, 0);
        }
    }

    /// Perform a single read, retrying on `EINTR`. Updates `self.end` on success.
    fn read_once(&mut self, reader: &mut impl Read) -> Result<(), FillError> {
        debug_assert!(self.end < self.buf.len());
        loop {
            match reader.read(&mut self.buf[self.end..]) {
                Ok(0) => return Err(FillError::Eof),
                Ok(n) => {
                    self.end += n;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(FillError::WouldBlock);
                }
                Err(e) => return Err(FillError::Io(e)),
            }
        }
    }

    /// Shift unconsumed data to the front of the buffer.
    /// Preserves the initialized space beyond `end` so subsequent reads
    /// don't need to zero-initialize again.
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
    ///
    /// Compacts when either all data is consumed, or the consumed prefix
    /// exceeds both `threshold` and half of total data read (`end`).
    pub fn maybe_compact(&mut self, threshold: usize) -> bool {
        if self.start > 0
            && (self.start == self.end || (self.start > threshold && self.start > self.end / 2))
        {
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

    /// Discard all data, keeping the buffer initialization and allocation.
    pub fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }
}
