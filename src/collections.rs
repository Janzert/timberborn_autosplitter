//! Reading .NET collections out of the game.
//!
//! Collection layouts belong to Unity's Mono BCL rather than to Timberborn, so
//! they move on a Unity upgrade rather than a game patch. Field offsets are
//! therefore resolved by name like everything else, via
//! [`Class::of_object`] — a generic instantiation such as `HashSet<string>`
//! cannot be looked up in an image by name, but any instance points at its own
//! class.
//!
//! What genuinely has to be hardcoded is the shape of the Mono runtime's own
//! object headers, which are not managed types and have no named fields. Those
//! are part of the Mono ABI and far more stable than a BCL implementation.

use asr::{
    game_engine::unity::mono::{Class, Module},
    string::ArrayWString,
    Address, Process,
};

/// `MonoArray`: object header, then bounds, then length, then the elements.
pub const ARRAY_LENGTH: u64 = 0x18;
pub const ARRAY_DATA: u64 = 0x20;

/// `MonoString`: object header, then the length, then UTF-16 characters.
const STRING_LENGTH: u64 = 0x10;
const STRING_CHARS: u64 = 0x14;

/// Longest string compared against. Template names are far shorter.
const MAX_CHARS: usize = 64;

/// `Slot<T>` for a reference `T`: `int _hashCode`, `int _next`, then the
/// reference. This is a plain consequence of the struct's declared fields and
/// 64-bit alignment, not an implementation detail that can drift independently
/// -- if Mono ever changed it, the fields themselves would have changed too and
/// the name lookups above would fail first.
const SLOT_SIZE: u64 = 16;
const SLOT_VALUE: u64 = 8;

/// Whether a .NET string at `address` is `expected`, optionally followed by a
/// `.`-separated suffix.
///
/// Live entities name their component cache `<template>.EntityComponent`, while
/// prefabs use the bare template name, so both have to match. Requiring a `.`
/// boundary stops `Forester.Folktails` matching a longer template that merely
/// starts the same way.
pub fn string_starts_with_segment(process: &Process, address: Address, expected: &str) -> bool {
    let Ok(len) = process.read::<i32>(address.add(STRING_LENGTH)) else {
        return false;
    };
    let wanted = expected.encode_utf16().count();
    if len < 0 || (len as usize) < wanted || len as usize > MAX_CHARS {
        return false;
    }
    let Ok(text) = process.read::<ArrayWString<MAX_CHARS>>(address.add(STRING_CHARS)) else {
        return false;
    };
    let chars = text.as_slice();
    if chars.len() < wanted || !chars.iter().copied().take(wanted).eq(expected.encode_utf16()) {
        return false;
    }
    chars.len() == wanted || chars[wanted] == u16::from(b'.')
}

/// Whether a .NET string at `address` equals `expected`.
///
/// The length is checked first, so a shorter string cannot match a prefix and
/// the character read only happens when it could succeed.
pub fn string_eq(process: &Process, address: Address, expected: &str) -> bool {
    let Ok(len) = process.read::<i32>(address.add(STRING_LENGTH)) else {
        return false;
    };
    if len < 0 || len as usize != expected.encode_utf16().count() || len as usize > MAX_CHARS {
        return false;
    }
    process
        .read::<ArrayWString<MAX_CHARS>>(address.add(STRING_CHARS))
        .is_ok_and(|s| s.matches_str(expected))
}

/// A `HashSet<T>` whose field offsets have been resolved from an instance.
///
/// Mono's implementation stores `Slot { int _hashCode; int _next; T _value; }`
/// in `_slots`, with occupied slots marked by a non-negative `_hashCode` and
/// `_lastIndex` bounding the used region.
pub struct HashSet {
    slots: Address,
    last_index: i32,
}

/// Offset of `_count` on a `HashSet<T>`, resolved once so the cheap "has
/// anything changed" check does not need the rest of the layout.
pub fn count_offset(process: &Process, module: &Module, address: Address) -> Option<u32> {
    if let Some(class) = Class::of_object(process, module, address) {
        if let Some(offset) = class.get_field_offset(process, module, "_count") {
            return Some(offset);
        }
    }
    HashSet::looks_like_a_set(process, address).then_some(HashSet::COUNT)
}

impl HashSet {
    /// Mono's `HashSet<T>` after the object header: the references first
    /// (`_buckets`, `_slots`, `_comparer`, `_siInfo`), then the ints
    /// (`_count`, `_lastIndex`, `_freeList`, `_version`).
    ///
    /// Read out of a live game rather than assumed -- an unlock set with four
    /// entries showed `_buckets` and `_slots` as 7-element arrays at +0x10 and
    /// +0x18, then `_count` 4 and `_lastIndex` 4 packed at +0x30, and
    /// `_freeList` -1 with `_version` 4 at +0x38. Used only when the class
    /// metadata cannot answer; see [`List::offsets`] for why that happens.
    const SLOTS: u32 = 0x18;
    pub(super) const COUNT: u32 = 0x30;
    const LAST_INDEX: u32 = 0x34;

    /// Offsets of `_slots` and `_lastIndex`, by name where possible.
    fn offsets(process: &Process, module: &Module, address: Address) -> Option<(u32, u32)> {
        if let Some(class) = Class::of_object(process, module, address) {
            let slots = class.get_field_offset(process, module, "_slots");
            let last_index = class.get_field_offset(process, module, "_lastIndex");
            if let (Some(slots), Some(last_index)) = (slots, last_index) {
                return Some((slots, last_index));
            }
        }
        Self::looks_like_a_set(process, address).then_some((Self::SLOTS, Self::LAST_INDEX))
    }

    /// Whether the object holds a plausible slot array, with a used region
    /// inside it and a count inside that.
    pub(super) fn looks_like_a_set(process: &Process, address: Address) -> bool {
        let Ok(slots) = process.read::<u64>(address.add(Self::SLOTS as u64)) else {
            return false;
        };
        let slots = Address::new(slots);
        if slots.is_null() {
            return false;
        }
        let Ok(capacity) = process.read::<i64>(slots.add(ARRAY_LENGTH)) else {
            return false;
        };
        let Ok(count) = process.read::<i32>(address.add(Self::COUNT as u64)) else {
            return false;
        };
        let Ok(last_index) = process.read::<i32>(address.add(Self::LAST_INDEX as u64)) else {
            return false;
        };
        capacity > 0
            && (0..=capacity).contains(&i64::from(last_index))
            && (0..=i64::from(last_index)).contains(&i64::from(count))
    }

    /// Reads the shape of the set at `address`. Returns `None` if it is not a
    /// hash set, or is not laid out the way we expect.
    pub fn read(process: &Process, module: &Module, address: Address) -> Option<Self> {
        let (slots_field, last_index_field) = Self::offsets(process, module, address)?;

        let slots = process
            .read_pointer(address.add(slots_field as u64), module.get_pointer_size())
            .ok()
            .filter(|a| !a.is_null())?;
        let last_index = process
            .read::<i32>(address.add(last_index_field as u64))
            .ok()?;

        let capacity = process.read::<i64>(slots.add(ARRAY_LENGTH)).ok()?;
        if capacity <= 0 || last_index < 0 || last_index as i64 > capacity {
            return None;
        }

        Some(Self {
            slots,
            last_index,
        })
    }

    /// Whether the set contains a string equal to `expected`.
    pub fn contains_str(&self, process: &Process, expected: &str) -> bool {
        for i in 0..self.last_index as u64 {
            let slot = self.slots.add(ARRAY_DATA + i * SLOT_SIZE);
            // A negative hash marks a free slot.
            if process.read::<i32>(slot).is_ok_and(|hash| hash < 0) {
                continue;
            }
            let Ok(value) = process.read::<u64>(slot.add(SLOT_VALUE)) else {
                continue;
            };
            let value = Address::new(value);
            if !value.is_null() && string_eq(process, value, expected) {
                return true;
            }
        }
        false
    }
}

/// A `List<T>` of references.
///
/// Only the offsets are needed: the elements are read directly out of the
/// backing array, which avoids resolving the class again per read.
pub struct List;

impl List {
    /// `List<T>` after the object header: `T[] _items`, then `int _size`, then
    /// `int _version`. Used only when the class metadata cannot answer, and
    /// only when the object agrees with it.
    const ITEMS: u32 = 0x10;
    const SIZE: u32 = 0x18;

    /// Offsets of `_size` and `_items`, by name where possible.
    ///
    /// Mono fills a class's field table in lazily, and for an inflated generic
    /// nothing necessarily has. Measured against a live game: `Class::of_object`
    /// resolved the class of `_entitiesInInstantiationOrder` and then reported
    /// *none* of `_size`, `_items` or `_version`, while the object itself was
    /// plainly a list -- 5032 entries in an 8192-slot array. The same lookup
    /// succeeded against a different process running the same build, so this is
    /// not something a version check could ever have caught.
    ///
    /// So the layout is the fallback, and it is only taken when the object
    /// reads like a list at those offsets: a real BCL change fails here rather
    /// than quietly yielding a wrong count.
    pub fn offsets(process: &Process, module: &Module, address: Address) -> Option<(u32, u32)> {
        if let Some(class) = Class::of_object(process, module, address) {
            let size = class.get_field_offset(process, module, "_size");
            let items = class.get_field_offset(process, module, "_items");
            if let (Some(size), Some(items)) = (size, items) {
                return Some((size, items));
            }
        }
        Self::looks_like_a_list(process, address)
            .then_some((Self::SIZE, Self::ITEMS))
    }

    /// Whether the object holds a plausible backing array and a count that fits
    /// inside it.
    fn looks_like_a_list(process: &Process, address: Address) -> bool {
        let Ok(items) = process.read::<u64>(address.add(Self::ITEMS as u64)) else {
            return false;
        };
        let items = Address::new(items);
        if items.is_null() {
            return false;
        }
        let Ok(capacity) = process.read::<i64>(items.add(ARRAY_LENGTH)) else {
            return false;
        };
        let Ok(size) = process.read::<i32>(address.add(Self::SIZE as u64)) else {
            return false;
        };
        capacity > 0 && size >= 0 && i64::from(size) <= capacity
    }
}
