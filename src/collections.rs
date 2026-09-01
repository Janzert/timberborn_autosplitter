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
    Class::of_object(process, module, address)?.get_field_offset(process, module, "_count")
}

impl HashSet {
    /// Reads the shape of the set at `address`. Returns `None` if it is not a
    /// hash set, or is not laid out the way we expect.
    pub fn read(process: &Process, module: &Module, address: Address) -> Option<Self> {
        let class = Class::of_object(process, module, address)?;

        let slots_field = class.get_field_offset(process, module, "_slots")?;
        let last_index_field = class.get_field_offset(process, module, "_lastIndex")?;

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
    /// Offset of `_size`. `_items` sits on the same class and is resolved
    /// alongside it by the caller.
    pub fn size_offset(process: &Process, module: &Module, address: Address) -> Option<u32> {
        Class::of_object(process, module, address)?.get_field_offset(process, module, "_size")
    }
}

