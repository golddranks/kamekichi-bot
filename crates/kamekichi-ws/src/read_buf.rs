//! Contiguous read buffer for streaming protocol parsing.
//!
//! # Layout
//!
//! ```text
//!  [ consumed | pending |   spare   | allocated ]
//!  0        start      end         len         cap
//! ```
//!
//! - **consumed** (`0..start`) — data already processed by the caller.
//!   [`consume`](ReadBuf::consume) advances `start` but does not discard
//!   the bytes — they remain accessible until the next
//!   [`compact`](ReadBuf::compact). This lets callers consume a frame
//!   header while still borrowing the payload without copying data out.
//! - **pending** (`start..end`) — data received but not yet processed.
//! - **spare** (`end..len`) — initialized memory available for the
//!   next `Read::read` call. `len` only shrinks via an explicit
//!   [`ReadBuf::maybe_shrink_capacity`] call, so zero-initializing
//!   the same region again and again is avoided.
//! - **allocated** (`len..cap`) — allocated but uninitialized part of the
//!   underlying allocation; can be initialized without reallocating.
//!
//! # Filling
//!
//! Two fill strategies:
//!
//! - [`ReadBuf::fill_from`] — for known lengths: loop until at least `need`
//!   pending bytes are available. Used for WebSocket frames
//!   where the payload length is known upfront.
//!
//! - [`ReadBuf::read_until`] — search-driven: read in a loop, invoking a
//!   callback after each read until it produces a value. Used for HTTP
//!   response header parsing where the terminator position is unknown.
//!
//! Both retry immediately on `EINTR`, treat `read() → 0` as EOF,
//! and return `WouldBlock` when the stream read would block or timeout,
//! to allow the caller to retry the operation later.
//!
//! # Memory lifecycle
//!
//! The buffer grows as needed during fills but never shrinks on its
//! own. The caller is responsible for reclaiming memory via
//! [`ReadBuf::compact`] (unconditional shift of pending data to offset 0),
//! [`ReadBuf::maybe_compact`] (compact only when the consumed prefix
//! exceeds a threshold), and [`ReadBuf::maybe_shrink_capacity`]
//! (probabilistically release excess backing allocation).
//! ([`ReadBuf::clear`] resets cursors without touching memory and is
//! not a reclamation method.)

use std::io::{self, Read};
use std::ops::Range;

use crate::rng::Rng;

/// Error from [`ReadBuf::read_until`].
#[derive(Debug)]
pub enum ReadUntilError<E> {
    /// The stream reached EOF before the callback produced a value.
    Eof,
    /// An IO error occurred while reading.
    Io(io::Error),
    /// The stream returned `WouldBlock` or `TimedOut`.
    WouldBlock,
    /// Pending data reached the caller's [`limit`](ReadBuf::read_until) before the callback produced a value.
    LimitReached,
    /// The callback error, passed through.
    CallbackError(E),
}

/// Error from [`ReadBuf::fill_from`].
#[derive(Debug)]
pub enum FillError {
    /// The stream reached EOF before enough data was available.
    Eof,
    /// An IO error occurred while reading.
    Io(io::Error),
    /// The stream returned `WouldBlock` or `TimedOut`.
    WouldBlock,
}

impl<E> From<FillError> for ReadUntilError<E> {
    fn from(e: FillError) -> Self {
        match e {
            FillError::Eof => ReadUntilError::Eof,
            FillError::WouldBlock => ReadUntilError::WouldBlock,
            FillError::Io(e) => ReadUntilError::Io(e),
        }
    }
}

/// The main buffer container. See [module docs](self) for layout and design.
pub struct ReadBuf {
    // Invariant: `start <= end <= buf.len()`.
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl std::fmt::Debug for ReadBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadBuf")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("len", &self.buf.len())
            .field("cap", &self.buf.capacity())
            .finish()
    }
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

    /// Number of unconsumed bytes in the buffer.
    pub fn pending_len(&self) -> usize {
        debug_assert!(self.start <= self.end);
        self.end - self.start
    }

    /// Unconsumed data in the buffer.
    pub fn pending(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    /// Initialized part of the backing buffer (including read-into
    /// and zero-initialized regions) at the given absolute range.
    ///
    /// Absolute offsets are invalidated by [`compact`](Self::compact).
    ///
    /// # Panics
    ///
    /// Panics if the range extends past the initialized region of the
    /// backing buffer.
    pub fn slice(&self, range: Range<usize>) -> &[u8] {
        &self.buf[range]
    }

    /// Absolute offset of the end of pending data in the backing buffer.
    ///
    /// Absolute offsets are invalidated by [`compact`](Self::compact).
    pub fn end_offset(&self) -> usize {
        self.end
    }

    /// Absolute offset of the start of pending data in the backing buffer.
    ///
    /// Absolute offsets are invalidated by [`compact`](Self::compact).
    pub fn start_offset(&self) -> usize {
        self.start
    }

    /// Advance the read cursor past `n` bytes of pending data.
    ///
    /// # Panics
    ///
    /// May or may not panic if `n` exceeds the pending data length.
    pub fn consume(&mut self, n: usize) {
        debug_assert!(
            n <= self.pending_len(),
            "consume({n}) exceeds pending length ({})",
            self.pending_len(),
        );
        self.start = self.end.min(self.start.saturating_add(n));
    }

    /// Ensure at least `need` bytes are available in [`pending()`](Self::pending),
    /// reading from `reader` as necessary.
    ///
    /// May read more than `need` bytes if the OS provides them — the extra
    /// data stays buffered for subsequent calls.
    ///
    /// On error, bytes already read within the same call remain in
    /// the buffer, so retrying with the same `need` resumes where
    /// the previous attempt left off.
    pub fn fill_from(&mut self, reader: &mut impl Read, need: usize) -> Result<(), FillError> {
        let target = self.start.saturating_add(need);
        if self.end >= target {
            return Ok(());
        }
        self.ensure_initialized(target);
        while self.end < target {
            self.read_once(reader)?;
        }
        Ok(())
    }

    /// Read from `reader` in a loop, calling `f` after each read
    /// until `f` returns `Ok(Some(value))` or an error occurs.
    /// `f` may return `Ok(None)` to request more data. `f` gets
    /// `&ReadBuf` as its argument, so it can inspect the current
    /// buffer contents and decide whether to continue or return
    /// a value. Note, however, that for implementing incremental
    /// scanning, the callback must manage the cursor position
    /// itself, as it can't call mutable methods on `ReadBuf`.
    ///
    /// If there is already pending data in the buffer (`start <
    /// end`), `f` is called once before the first read.
    ///
    /// `limit` bounds how long the search continues: once at least
    /// `limit` bytes of pending data have accumulated and `f`
    /// hasn't produced a value, [`ReadUntilError::LimitReached`] is
    /// returned. A single read may buffer data beyond `limit`;
    /// that extra is available in the pending section, and the
    /// data is meant to remain available for later operations.
    ///
    /// On error, bytes already read within the same call remain in
    /// the buffer, and a retry will check them before reading.
    pub fn read_until<T, E>(
        &mut self,
        reader: &mut impl Read,
        limit: usize,
        mut f: impl FnMut(&ReadBuf) -> Result<Option<T>, E>,
    ) -> Result<T, ReadUntilError<E>> {
        // Check pre-existing data before the first (potentially blocking) read.
        if self.end > self.start
            && let Some(v) = f(self).map_err(ReadUntilError::CallbackError)?
        {
            return Ok(v);
        }
        let target = self.start.saturating_add(limit);
        if self.end >= target {
            return Err(ReadUntilError::LimitReached);
        }
        self.ensure_initialized(target);
        // `read_once` requires `end < buf.len()`: entering the loop
        // (and each subsequent iteration) we have `end < target`, and
        // `ensure_initialized` ensures `target < buf.len()`, so
        // end < target < buf.len() holds. `target` is fixed throughout,
        // and `f` cannot modify immutable `&ReadBuf`.
        loop {
            self.read_once(reader)?;
            if let Some(v) = f(self).map_err(ReadUntilError::CallbackError)? {
                return Ok(v);
            }
            if self.end >= target {
                return Err(ReadUntilError::LimitReached);
            }
        }
    }

    /// Ensure the buffer is initialized to at least `target`.
    /// May initialize more than requested for efficiency.
    fn ensure_initialized(&mut self, target: usize) {
        /// Floor for the readable slice reserved at the start of a read loop
        /// to avoid tiny reads.
        const MIN_READ_SLICE: usize = 4096;

        /// Extra headroom past the requested target, so reads closing near the
        /// target still get reasonably sized slices.
        const MIN_READ_HEADROOM: usize = 512;

        let needed = target
            .saturating_add(MIN_READ_HEADROOM)
            .max(self.end.saturating_add(MIN_READ_SLICE));
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
                    let slice_len = self.buf.len() - self.end;
                    assert!(
                        n <= slice_len,
                        "Buggy Read::read returned {n} bytes into a {slice_len}-byte slice",
                    );
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

    /// Shift unconsumed ("pending") data to the front of the buffer,
    /// overwriting the "consumed" portion. The spare region beyond
    /// the new `end` remains initialized so subsequent reads don't
    /// re-zero. When pending data was shifted, the spare's byte
    /// contents are left shuffled (the old consumed/pending/spare
    /// data not overwritten remains untouched).
    pub fn compact(&mut self) {
        if self.start == 0 {
            return;
        } else if self.start == self.end {
            self.clear();
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
        if self.start > 0 && (self.start == self.end || self.start > threshold.max(self.end / 2)) {
            self.compact();
            true
        } else {
            false
        }
    }

    /// Probabilistically shrink the backing allocation down toward `max_cap`
    /// if capacity exceeds it, to amortize reallocation cost when buffer
    /// size spikes repeatedly. Should be called after compacting.
    ///
    /// # Panics
    ///
    /// May or may not panic if not compacted, i.e. `start > 0`.
    pub fn maybe_shrink_capacity(&mut self, max_cap: usize, rng: &mut impl Rng) {
        debug_assert!(self.start == 0, "call compact() before shrinking");
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
