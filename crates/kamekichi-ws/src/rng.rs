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

#[cfg(all(feature = "rand", test))]
mod tests {
    use super::Rng;
    use std::convert::Infallible;
    struct Dummy;

    impl rand_core::TryRng for Dummy {
        type Error = Infallible;

        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            Ok(1)
        }

        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            unreachable!()
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
            for b in dest {
                *b = 0xAA;
            }
            Ok(())
        }
    }

    #[test]
    fn test_rand_core_impl_bridge() {
        let mut buf = [0u8; 4];
        Rng::fill_bytes(&mut Dummy, &mut buf);
        assert_eq!(buf, [0xAA; 4]);

        assert_eq!(Rng::next_u32(&mut Dummy), 1);
    }
}
