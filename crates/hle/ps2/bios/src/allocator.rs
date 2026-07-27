// SPDX-License-Identifier: LGPL-2.1-or-later

use crate::{FixedTable, KernelError};

/// IOP `sysmem` allocation quantum.
pub const SYSMEM_QUANTUM: u32 = 256;
const MAX_ALLOCATIONS: usize = crate::DEFAULT_ALLOCATION_CAPACITY;

/// Placement mode accepted by `AllocSysMemory`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AllocationMode {
    /// Lowest-address suitable block (`ALLOC_FIRST`).
    First = 0,
    /// Highest-address suitable block (`ALLOC_LAST`).
    Last = 1,
    /// Exact caller-selected address (`ALLOC_ADDRESS`).
    Address = 2,
}

impl TryFrom<u32> for AllocationMode {
    type Error = KernelError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::First),
            1 => Ok(Self::Last),
            2 => Ok(Self::Address),
            _ => Err(KernelError::IllegalMemoryMode),
        }
    }
}

/// One live guest system-memory allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Allocation {
    /// First guest address.
    pub address: u32,
    /// Reserved byte count, rounded to the system-memory quantum.
    pub size: u32,
    /// Caller-requested byte count.
    pub requested_size: u32,
    /// Guaranteed starting-address alignment.
    pub alignment: u32,
}

impl Allocation {
    fn end(self) -> u32 {
        self.address + self.size
    }
}

/// Deterministic fixed-capacity IOP system-memory allocator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemMemory {
    start: u32,
    end: u32,
    allocations: FixedTable<Allocation, MAX_ALLOCATIONS>,
}

impl SystemMemory {
    /// Creates an empty, quantum-aligned arena.
    ///
    /// # Errors
    ///
    /// Returns [`KernelError::IllegalAddress`] for an invalid arena.
    pub fn new(start: u32, end: u32) -> Result<Self, KernelError> {
        if start == 0 || start >= end || start % SYSMEM_QUANTUM != 0 || end % SYSMEM_QUANTUM != 0 {
            return Err(KernelError::IllegalAddress);
        }
        Ok(Self {
            start,
            end,
            allocations: FixedTable::new(1),
        })
    }

    /// Drops all allocations and restores the full arena.
    pub fn reset(&mut self) {
        self.allocations.clear();
    }

    /// Returns the arena bounds.
    #[must_use]
    pub const fn range(&self) -> (u32, u32) {
        (self.start, self.end)
    }

    /// Allocates with the standard 256-byte alignment.
    ///
    /// `address` is consulted only for [`AllocationMode::Address`].
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible size, address, or exhaustion result.
    pub fn allocate(
        &mut self,
        mode: AllocationMode,
        size: u32,
        address: u32,
    ) -> Result<Allocation, KernelError> {
        self.allocate_aligned(mode, size, address, SYSMEM_QUANTUM)
    }

    /// Allocates a quantum-sized block with a stronger power-of-two alignment.
    ///
    /// # Errors
    ///
    /// Returns a BIOS-compatible size, address, or exhaustion result.
    pub fn allocate_aligned(
        &mut self,
        mode: AllocationMode,
        size: u32,
        address: u32,
        alignment: u32,
    ) -> Result<Allocation, KernelError> {
        if size == 0 {
            return Err(KernelError::IllegalSize);
        }
        if alignment < SYSMEM_QUANTUM || !alignment.is_power_of_two() {
            return Err(KernelError::IllegalAddress);
        }
        if self.allocations.next_id().is_none() {
            return Err(KernelError::NoMemory);
        }
        let reserved = align_up(size, SYSMEM_QUANTUM).ok_or(KernelError::IllegalSize)?;
        let selected = match mode {
            AllocationMode::First => self.find_first(reserved, alignment),
            AllocationMode::Last => self.find_last(reserved, alignment),
            AllocationMode::Address => {
                if address % alignment != 0 {
                    return Err(KernelError::IllegalAddress);
                }
                self.is_free(address, reserved).then_some(address)
            }
        }
        .ok_or(KernelError::NoMemory)?;
        let allocation = Allocation {
            address: selected,
            size: reserved,
            requested_size: size,
            alignment,
        };
        self.allocations.insert(allocation)?;
        Ok(allocation)
    }

    /// Frees the allocation beginning at an exact block address.
    ///
    /// # Errors
    ///
    /// Returns [`KernelError::IllegalAddress`] for an unknown block.
    pub fn free(&mut self, address: u32) -> Result<Allocation, KernelError> {
        let id = self
            .allocations
            .iter()
            .find_map(|(id, allocation)| (allocation.address == address).then_some(id))
            .ok_or(KernelError::IllegalAddress)?;
        self.allocations
            .remove(id)
            .ok_or(KernelError::IllegalAddress)
    }

    /// Returns the live block beginning at `address`.
    #[must_use]
    pub fn block(&self, address: u32) -> Option<Allocation> {
        self.allocations
            .iter()
            .find_map(|(_, allocation)| (allocation.address == address).then_some(*allocation))
    }

    /// Returns the complete arena byte count.
    #[must_use]
    pub const fn memory_size(&self) -> u32 {
        self.end - self.start
    }

    /// Returns the number of unallocated bytes.
    #[must_use]
    pub fn total_free(&self) -> u32 {
        self.memory_size()
            - self
                .allocations
                .iter()
                .map(|(_, allocation)| allocation.size)
                .sum::<u32>()
    }

    /// Returns the largest contiguous free block.
    #[must_use]
    pub fn maximum_free(&self) -> u32 {
        let mut maximum = 0;
        let mut cursor = self.start;
        while cursor < self.end {
            let next = self
                .allocations
                .iter()
                .filter(|(_, allocation)| allocation.address >= cursor)
                .map(|(_, allocation)| *allocation)
                .min_by_key(|allocation| allocation.address);
            if let Some(allocation) = next {
                maximum = maximum.max(allocation.address - cursor);
                cursor = allocation.end();
            } else {
                maximum = maximum.max(self.end - cursor);
                break;
            }
        }
        maximum
    }

    /// Returns occupied blocks in ascending address order.
    #[must_use]
    pub fn allocations(&self) -> Vec<Allocation> {
        let mut allocations: Vec<_> = self
            .allocations
            .iter()
            .map(|(_, allocation)| *allocation)
            .collect();
        allocations.sort_unstable_by_key(|allocation| allocation.address);
        allocations
    }

    fn find_first(&self, size: u32, alignment: u32) -> Option<u32> {
        let mut cursor = align_up(self.start, alignment)?;
        for allocation in self.allocations() {
            if cursor.checked_add(size)? <= allocation.address {
                return Some(cursor);
            }
            cursor = align_up(cursor.max(allocation.end()), alignment)?;
        }
        (cursor.checked_add(size)? <= self.end).then_some(cursor)
    }

    fn find_last(&self, size: u32, alignment: u32) -> Option<u32> {
        let mut boundary = self.end;
        for allocation in self.allocations().into_iter().rev() {
            let candidate = align_down(boundary.checked_sub(size)?, alignment);
            if candidate >= allocation.end() {
                return Some(candidate);
            }
            boundary = boundary.min(allocation.address);
        }
        let candidate = align_down(boundary.checked_sub(size)?, alignment);
        (candidate >= self.start).then_some(candidate)
    }

    fn is_free(&self, address: u32, size: u32) -> bool {
        let Some(end) = address.checked_add(size) else {
            return false;
        };
        address >= self.start
            && end <= self.end
            && self
                .allocations
                .iter()
                .all(|(_, allocation)| end <= allocation.address || address >= allocation.end())
    }
}

fn align_up(value: u32, alignment: u32) -> Option<u32> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

const fn align_down(value: u32, alignment: u32) -> u32 {
    value & !(alignment - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_last_address_and_coalescing_are_deterministic() {
        let mut memory = SystemMemory::new(0x1000, 0x2000).unwrap();
        let first = memory.allocate(AllocationMode::First, 1, 0).unwrap();
        let last = memory.allocate(AllocationMode::Last, 257, 0).unwrap();
        let exact = memory
            .allocate(AllocationMode::Address, 0x100, 0x1500)
            .unwrap();
        assert_eq!(first.address, 0x1000);
        assert_eq!(first.size, 0x100);
        assert_eq!(last.address, 0x1e00);
        assert_eq!(exact.address, 0x1500);
        assert_eq!(memory.total_free(), 0xc00);
        assert_eq!(memory.maximum_free(), 0x800);

        memory.free(exact.address).unwrap();
        memory.free(first.address).unwrap();
        memory.free(last.address).unwrap();
        assert_eq!(memory.total_free(), 0x1000);
        assert_eq!(memory.maximum_free(), 0x1000);
        assert!(memory.allocations().is_empty());
    }

    #[test]
    fn allocation_rejects_invalid_inputs_without_state_change() {
        let mut memory = SystemMemory::new(0x1000, 0x2000).unwrap();
        assert_eq!(
            memory.allocate(AllocationMode::First, 0, 0),
            Err(KernelError::IllegalSize)
        );
        assert_eq!(
            memory.allocate(AllocationMode::Address, 1, 0x1080),
            Err(KernelError::IllegalAddress)
        );
        assert_eq!(
            memory.allocate(AllocationMode::Address, 0x2000, 0x1000),
            Err(KernelError::NoMemory)
        );
        assert_eq!(memory.total_free(), 0x1000);
    }

    #[test]
    fn stronger_alignment_is_preserved() {
        let mut memory = SystemMemory::new(0x1000, 0x5000).unwrap();
        memory.allocate(AllocationMode::First, 0x100, 0).unwrap();
        let aligned = memory
            .allocate_aligned(AllocationMode::First, 0x100, 0, 0x1000)
            .unwrap();
        assert_eq!(aligned.address, 0x2000);
        assert_eq!(aligned.alignment, 0x1000);
    }
}
