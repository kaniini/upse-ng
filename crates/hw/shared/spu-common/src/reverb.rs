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
        let length = i128::try_from(self.length).unwrap_or(i128::MAX);
        let position = i128::try_from(self.position).unwrap_or(0);
        let offset = i128::from(offset);
        let wrapped = (position + offset).rem_euclid(length);
        self.position = usize::try_from(wrapped).unwrap_or(0);
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
    }
}
