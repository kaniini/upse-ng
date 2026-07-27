// SPDX-License-Identifier: LGPL-2.1-or-later

use crate::KernelError;

/// Instance-owned fixed-capacity table with stable positive identifiers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixedTable<T, const N: usize> {
    entries: [Option<T>; N],
    first_id: u32,
    len: usize,
}

impl<T, const N: usize> FixedTable<T, N> {
    /// Creates an empty table whose first slot has `first_id`.
    #[must_use]
    pub fn new(first_id: u32) -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            first_id,
            len: 0,
        }
    }

    /// Removes every entry without changing identifier assignment.
    pub fn clear(&mut self) {
        for entry in &mut self.entries {
            *entry = None;
        }
        self.len = 0;
    }

    /// Returns the fixed maximum number of entries.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Returns the number of occupied entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Reports whether the table is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the next identifier that insertion would use.
    #[must_use]
    pub fn next_id(&self) -> Option<u32> {
        self.entries
            .iter()
            .position(Option::is_none)
            .and_then(|index| self.first_id.checked_add(u32::try_from(index).ok()?))
    }

    /// Inserts into the lowest available slot.
    ///
    /// # Errors
    ///
    /// Returns [`KernelError::NoMemory`] when all slots are occupied.
    pub fn insert(&mut self, value: T) -> Result<u32, KernelError> {
        let id = self.next_id().ok_or(KernelError::NoMemory)?;
        self.insert_at(id, value)?;
        Ok(id)
    }

    /// Inserts into a specific unoccupied slot.
    ///
    /// # Errors
    ///
    /// Returns [`KernelError::IllegalId`] for an out-of-range or occupied slot.
    pub fn insert_at(&mut self, id: u32, value: T) -> Result<(), KernelError> {
        let index = self.index(id).ok_or(KernelError::IllegalId)?;
        if self.entries[index].is_some() {
            return Err(KernelError::IllegalId);
        }
        self.entries[index] = Some(value);
        self.len += 1;
        Ok(())
    }

    /// Returns one table entry.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&T> {
        self.entries.get(self.index(id)?)?.as_ref()
    }

    /// Returns one mutable table entry.
    #[must_use]
    pub fn get_mut(&mut self, id: u32) -> Option<&mut T> {
        let index = self.index(id)?;
        self.entries.get_mut(index)?.as_mut()
    }

    /// Removes one entry.
    #[must_use]
    pub fn remove(&mut self, id: u32) -> Option<T> {
        let index = self.index(id)?;
        let removed = self.entries.get_mut(index)?.take();
        if removed.is_some() {
            self.len -= 1;
        }
        removed
    }

    /// Iterates over occupied entries in identifier order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        let first_id = self.first_id;
        self.entries
            .iter()
            .enumerate()
            .filter_map(move |(index, entry)| {
                let value = entry.as_ref()?;
                let id = first_id.checked_add(u32::try_from(index).ok()?)?;
                Some((id, value))
            })
    }

    /// Iterates mutably over occupied entries in identifier order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (u32, &mut T)> {
        let first_id = self.first_id;
        self.entries
            .iter_mut()
            .enumerate()
            .filter_map(move |(index, entry)| {
                let value = entry.as_mut()?;
                let id = first_id.checked_add(u32::try_from(index).ok()?)?;
                Some((id, value))
            })
    }

    fn index(&self, id: u32) -> Option<usize> {
        let index = usize::try_from(id.checked_sub(self.first_id)?).ok()?;
        (index < N).then_some(index)
    }
}
