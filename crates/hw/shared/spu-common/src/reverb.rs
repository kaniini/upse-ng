// SPDX-License-Identifier: LGPL-2.1-or-later
//! Register-free circular-address arithmetic used by sound RAM effects.

/// Circular byte address constrained to a caller-supplied effects area.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingBufferAddress {
    base: usize,
    length: usize,
    position: usize,
}

impl RingBufferAddress {
    /// Constructs an address when the ring length is nonzero.
    #[must_use]
    pub fn new(base: usize, length: usize) -> Option<Self> {
        if length == 0 {
            return None;
        }
        Some(Self {
            base,
            length,
            position: 0,
        })
    }

    /// Returns the current absolute byte address.
    #[must_use]
    pub fn get(self) -> Option<usize> {
        self.base.checked_add(self.position)
    }

    /// Advances by a signed relative offset with deterministic Euclidean wrap.
    pub fn advance(&mut self, offset: i64) {
        let length = u64::try_from(self.length).unwrap_or(u64::MAX);
        let distance = usize::try_from(offset.unsigned_abs() % length).unwrap_or(0);
        if offset >= 0 {
            let remaining = self.length - self.position;
            self.position = if distance >= remaining {
                distance - remaining
            } else {
                self.position + distance
            };
        } else {
            self.position = if distance > self.position {
                self.length - (distance - self.position)
            } else {
                self.position - distance
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RingBufferAddress;

    #[test]
    fn positive_and_negative_offsets_wrap_without_address_policy() {
        assert_eq!(RingBufferAddress::new(10, 0), None);
        let mut address = RingBufferAddress::new(1_000, 16).unwrap();
        address.advance(18);
        assert_eq!(address.get(), Some(1_002));
        address.advance(-5);
        assert_eq!(address.get(), Some(1_013));
        address.advance(i64::MIN);
        assert_eq!(address.get(), Some(1_013));
    }

    #[test]
    fn signed_wrap_matches_euclidean_remainder() {
        for length in 1..=65 {
            for initial in 0..length {
                for offset in [
                    i64::MIN,
                    -1_000_003,
                    -129,
                    -65,
                    -1,
                    0,
                    1,
                    65,
                    129,
                    1_000_003,
                    i64::MAX,
                ] {
                    let mut address = RingBufferAddress::new(0, length).unwrap();
                    address.advance(i64::try_from(initial).unwrap());
                    address.advance(offset);
                    let expected = (i128::try_from(initial).unwrap() + i128::from(offset))
                        .rem_euclid(i128::try_from(length).unwrap());
                    assert_eq!(address.get(), usize::try_from(expected).ok());
                }
            }
        }
    }
}
