//! Generation-checked frame addresses. No pointers into the arena escape a borrow.
//!
//! A handle is 8 bytes (a `u32` index and a non-zero `u32` generation), so an
//! `Option<Handle>` is 8 bytes too. A value carries its generation itself
//! (`Generational`), so a slot is exactly its value and a lookup reads one
//! place.

use std::num::NonZeroU32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Handle {
  index: u32,
  generation: NonZeroU32,
}

/// A value the arena stores: it keeps its slot's generation inside itself
/// (in padding the value has anyway), so a lookup checks the generation and
/// reads the value from one place.
pub(crate) trait Generational {
  fn generation(&self) -> NonZeroU32;
  fn set_generation(&mut self, generation: NonZeroU32);
}

pub(crate) struct Arena<T> {
  slots: Vec<Option<T>>,
  /// Free slots with the generation their next value gets.
  free: Vec<(u32, NonZeroU32)>,
}

impl<T> Default for Arena<T> {
  fn default() -> Self {
    Self {
      slots: Vec::new(),
      free: Vec::new(),
    }
  }
}

const FIRST: NonZeroU32 = NonZeroU32::MIN;

impl<T: Generational> Arena<T> {
  /// Reserve a known subtree without a transient doubling allocation.
  pub fn reserve(&mut self, additional: usize) {
    self
      .slots
      .reserve_exact(additional.saturating_sub(self.free.len()));
  }

  pub fn insert(&mut self, mut value: T) -> Handle {
    let (index, generation) = self.free.pop().unwrap_or_else(|| {
      self.slots.push(None);
      (
        u32::try_from(self.slots.len() - 1).expect("frame arena index fits u32"),
        FIRST,
      )
    });
    value.set_generation(generation);
    self.slots[index as usize] = Some(value);
    Handle { index, generation }
  }

  pub fn get(&self, handle: Handle) -> Option<&T> {
    self
      .slots
      .get(handle.index as usize)?
      .as_ref()
      .filter(|v| v.generation() == handle.generation)
  }

  pub fn get_mut(&mut self, handle: Handle) -> Option<&mut T> {
    self
      .slots
      .get_mut(handle.index as usize)?
      .as_mut()
      .filter(|v| v.generation() == handle.generation)
  }

  pub fn remove(&mut self, handle: Handle) -> Option<T> {
    let slot = self.slots.get_mut(handle.index as usize)?;
    if slot.as_ref()?.generation() != handle.generation {
      return None;
    }
    let value = slot.take()?;
    // A generation can never wrap and revive a stale handle. Retire on overflow.
    if let Some(next) = handle.generation.checked_add(1) {
      self.free.push((handle.index, next));
    }
    Some(value)
  }

  pub fn values(&self) -> impl Iterator<Item = &T> {
    self.slots.iter().filter_map(|s| s.as_ref())
  }

  pub fn capacity_bytes(&self) -> usize {
    self.slots.capacity() * std::mem::size_of::<Option<T>>()
      + self.free.capacity() * std::mem::size_of::<(u32, NonZeroU32)>()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A test value carrying its generation beside its payload.
  #[derive(Debug, PartialEq)]
  struct G<T>(T, NonZeroU32);
  impl<T> Generational for G<T> {
    fn generation(&self) -> NonZeroU32 {
      self.1
    }
    fn set_generation(&mut self, generation: NonZeroU32) {
      self.1 = generation;
    }
  }
  fn g<T>(v: T) -> G<T> {
    G(v, FIRST)
  }

  #[test]
  fn handles_are_one_word() {
    assert_eq!(std::mem::size_of::<Handle>(), 8);
    assert_eq!(std::mem::size_of::<Option<Handle>>(), 8);
  }

  #[test]
  fn stale_handles_cannot_read_mutate_or_remove_reused_slots() {
    let mut arena = Arena::default();
    let old = arena.insert(g(String::from("old")));
    assert_eq!(arena.remove(old).map(|v| v.0).as_deref(), Some("old"));
    // Moving the backing allocation must not revive the removed handle.
    arena.reserve(128);
    let new = arena.insert(g(String::from("new")));
    assert_eq!(old.index, new.index);
    assert!(arena.get(old).is_none());
    assert!(arena.get_mut(old).is_none());
    assert!(arena.remove(old).is_none());
    assert_eq!(arena.get(new).map(|v| v.0.as_str()), Some("new"));
    for _ in 0..1000 {
      let h = arena.insert(g(vec![1u8; 64].len().to_string()));
      arena.remove(h).unwrap();
      assert!(arena.get(h).is_none());
    }
    assert_eq!(arena.values().count(), 1);
  }

  #[test]
  fn exhausted_generation_retires_slot() {
    let mut arena = Arena::default();
    let mut h = arena.insert(g(7));
    arena.get_mut(h).unwrap().set_generation(NonZeroU32::MAX);
    h.generation = NonZeroU32::MAX;
    assert_eq!(arena.remove(h).map(|v| v.0), Some(7));
    let fresh = arena.insert(g(8));
    assert_ne!(fresh.index, h.index);
    assert!(arena.get(h).is_none());
  }
}
