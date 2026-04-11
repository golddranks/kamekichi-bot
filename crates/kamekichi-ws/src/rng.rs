/// A trait for the random number generator used by the WebSocket implementation.
/// This is basically a wrapper around [`rand_core::Rng`], but as rand_core is
/// not at version 1.0, so we don't want to have its types in public API.
pub trait Rng {
    /// Fill `buf` with random bytes.
    fn fill_bytes(&mut self, buf: &mut [u8]);
    /// Return a random `u32`.
    fn next_u32(&mut self) -> u32;
    /// Return `true` with roughly 1-in-8 probability.
    fn one_in_eight_odds(&mut self) -> bool {
        self.next_u32() & 0b111 == 0
    }
}

#[cfg(feature = "rand")]
impl<R> Rng for R
where
    R: rand_core::Rng,
{
    fn fill_bytes(&mut self, buf: &mut [u8]) {
        rand_core::Rng::fill_bytes(self, buf)
    }

    fn next_u32(&mut self) -> u32 {
        rand_core::Rng::next_u32(self)
    }
}
