//! Fixed storage for the live child sessions owned by one async coordinator.

/// Fixed-capacity storage with O(1) insertion, lookup, and retirement.
///
/// The coordinator owns this table and moves child sessions into it as it
/// pulls intents through [Proxima's `Pipe` primitive](https://github.com/brianbruggeman/proxima/blob/6cffdc693bbd34483f1349f99b0ebd282c92fca4/proxima-primitives/src/pipe/primitives.rs#L91-L113).
/// It removes a session when that child's completion event arrives,
/// then pulls the next intent. This stores lifecycle state directly; it is not
/// a permit gate or a work queue.
pub struct SessionSlots<T, const CAPACITY: usize> {
    entries: [Entry<T>; CAPACITY],
    free_head: Option<usize>,
    length: usize,
}

/// A generation checked handle to one live child-session slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSlotId {
    index: usize,
    generation: u64,
}

impl SessionSlotId {
    /// Index used for O(1) access within the owning session table.
    #[must_use]
    pub const fn index(self) -> usize {
        self.index
    }
}

enum Entry<T> {
    Vacant {
        next: Option<usize>,
        generation: u64,
    },
    Occupied {
        value: T,
        generation: u64,
    },
    Retired,
}

impl<T, const CAPACITY: usize> SessionSlots<T, CAPACITY> {
    /// Creates an empty table whose storage is inline and fixed at compile time.
    #[must_use]
    pub fn new() -> Self {
        let entries = core::array::from_fn(|index| {
            let next = (index + 1 < CAPACITY).then_some(index + 1);
            Entry::Vacant {
                next,
                generation: 0,
            }
        });
        let free_head = (CAPACITY > 0).then_some(0);
        Self {
            entries,
            free_head,
            length: 0,
        }
    }

    /// Inserts one live session, returning its stable slot index.
    ///
    /// Returns the unchanged value when the table has no free slot.
    ///
    /// # Errors
    ///
    /// Returns the unchanged value when the table has no free slot.
    pub fn insert(&mut self, value: T) -> Result<SessionSlotId, T> {
        let Some(index) = self.free_head else {
            return Err(value);
        };
        let (next, generation) = match &self.entries[index] {
            Entry::Vacant { next, generation } => (*next, *generation),
            Entry::Occupied { .. } | Entry::Retired => return Err(value),
        };
        self.free_head = next;
        self.entries[index] = Entry::Occupied { value, generation };
        self.length += 1;
        Ok(SessionSlotId { index, generation })
    }

    /// Returns the live session stored at `index`.
    #[must_use]
    pub fn get(&self, slot: SessionSlotId) -> Option<&T> {
        match self.entries.get(slot.index)? {
            Entry::Occupied { value, generation } if *generation == slot.generation => Some(value),
            Entry::Vacant { .. } | Entry::Occupied { .. } | Entry::Retired => None,
        }
    }

    /// Returns mutable access to the live session stored at `index`.
    pub fn get_mut(&mut self, slot: SessionSlotId) -> Option<&mut T> {
        match self.entries.get_mut(slot.index)? {
            Entry::Occupied { value, generation } if *generation == slot.generation => Some(value),
            Entry::Vacant { .. } | Entry::Occupied { .. } | Entry::Retired => None,
        }
    }

    /// Removes and returns one completed session in O(1).
    pub fn remove(&mut self, slot: SessionSlotId) -> Option<T> {
        let entry = self.entries.get_mut(slot.index)?;
        match core::mem::replace(entry, Entry::Retired) {
            Entry::Vacant { next, generation } => {
                *entry = Entry::Vacant { next, generation };
                None
            }
            Entry::Occupied { value, generation } if generation == slot.generation => {
                self.length -= 1;
                if let Some(next_generation) = generation.checked_add(1) {
                    *entry = Entry::Vacant {
                        next: self.free_head,
                        generation: next_generation,
                    };
                    self.free_head = Some(slot.index);
                }
                Some(value)
            }
            occupied @ Entry::Occupied { .. } => {
                *entry = occupied;
                None
            }
            Entry::Retired => None,
        }
    }

    /// Number of live sessions currently stored.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    /// Compile-time capacity of the table.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    /// Whether the table has no live sessions.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Whether no session slot is available for insertion.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.free_head.is_none()
    }

    /// Scans live values within fixed capacity without a second index or move.
    #[must_use]
    pub fn any(&self, mut predicate: impl FnMut(&T) -> bool) -> bool {
        self.entries.iter().any(|entry| match entry {
            Entry::Occupied { value, .. } => predicate(value),
            Entry::Vacant { .. } | Entry::Retired => false,
        })
    }
}

impl<T, const CAPACITY: usize> Default for SessionSlots<T, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}
