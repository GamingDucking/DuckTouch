/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use super::*;
use crate::mem::{MutVoidPtr, PAGE_SIZE};

fn memory(size: u32) -> (Mem, u32) {
    let mut mem = Mem::new();
    mem.set_null_segment_size(PAGE_SIZE);
    let ptr = mem.alloc(size);
    mem.bytes_at_mut(ptr.cast(), size).fill(0xAA);
    (mem, ptr.to_bits())
}

fn result(mem: &mut Mem, addr: u32, vtype: VType, value: &str) -> SearchResult {
    let bits = vtype.parse(value).unwrap();
    assert!(vtype.write_at(mem, addr, bits));
    SearchResult { addr, vtype, bits, changed: false }
}

fn snapshot(mem: &Mem, addr: u32, size: u32) -> Vec<u8> {
    mem.get_bytes_fallible(ConstVoidPtr::from_bits(addr), size).unwrap().to_vec()
}

#[test]
fn values_must_fit_their_integer_type() {
    for (t, min, max, below, above) in [
        (VType::U8, "0", "255", "-1", "256"),
        (VType::I8, "-128", "127", "-129", "128"),
        (VType::U16, "0", "65535", "-1", "65536"),
        (VType::I16, "-32768", "32767", "-32769", "32768"),
        (VType::U32, "0", "4294967295", "-1", "4294967296"),
        (VType::I32, "-2147483648", "2147483647", "-2147483649", "2147483648"),
    ] {
        assert!(t.parse(min).is_some(), "{t:?}");
        assert!(t.parse(max).is_some(), "{t:?}");
        assert_eq!(t.parse(below), None, "{t:?}");
        assert_eq!(t.parse(above), None, "{t:?}");
    }
    assert_eq!(VType::U8.parse("600"), None);
    assert_eq!(VType::I8.parse("0xff"), Some(255));
    assert_eq!(VType::I8.parse("0x100"), None);
    assert_eq!(VType::I16.parse("-1"), Some(65535));
}

#[test]
fn float_input_is_numeric_not_an_integer_bit_pattern() {
    assert_eq!(VType::F32.parse("600"), Some(600.0f32.to_bits() as u64));
    assert_eq!(VType::F32.parse("12.5"), Some(12.5f32.to_bits() as u64));
    assert_eq!(VType::F32.parse("-1.5"), Some((-1.5f32).to_bits() as u64));
    for invalid in ["NaN", "inf", "1e100", ""] {
        assert_eq!(VType::F32.parse(invalid), None);
    }
}

#[test]
fn auto_search_does_not_find_truncated_values() {
    let (mut mem, base) = memory(64);
    let integer = result(&mut mem, base, VType::I32, "600");
    let float = result(&mut mem, base + 8, VType::F32, "600");
    let byte = result(&mut mem, base + 16, VType::U8, "88");
    let hits = search_all(&mem, VType::Auto, "600", None);
    assert!(hits.iter().any(|r| r.addr == integer.addr && r.vtype == VType::I32));
    assert!(hits.iter().any(|r| r.addr == float.addr && r.vtype == VType::F32));
    assert!(!hits.iter().any(|r| matches!(r.vtype, VType::U8 | VType::I8)));
    let refined = search_all(&mem, VType::Auto, "600", Some(&[integer, float, byte]));
    assert_eq!(refined.len(), 2);
    assert_eq!(refined[0].addr, integer.addr);
    assert_eq!(refined[1].addr, float.addr);
}

#[test]
fn empty_refine_never_restarts_a_search() {
    let (mut mem, base) = memory(64);
    result(&mut mem, base, VType::I32, "600");
    for t in [VType::I32, VType::Auto] {
        assert!(search_all(&mem, t, "600", Some(&[])).is_empty());
        assert!(!search_all(&mem, t, "600", None).is_empty());
    }
}

#[test]
fn bulk_range_failure_does_not_partially_write() {
    let (mut mem, base) = memory(64);
    let mut hits = [
        result(&mut mem, base, VType::I32, "100"),
        result(&mut mem, base + 8, VType::U8, "100"),
    ];
    let before = snapshot(&mem, base, 64);
    assert!(set_all(&mut mem, &mut hits, VType::Auto, "999999").is_err());
    assert_eq!(snapshot(&mem, base, 64), before);
    assert_eq!(hits[0].bits, 100);
}

#[test]
fn changing_ui_type_cannot_widen_a_bulk_write() {
    let (mut mem, base) = memory(64);
    let mut hits = [result(&mut mem, base, VType::U8, "100")];
    let before = snapshot(&mem, base, 64);
    assert!(set_all(&mut mem, &mut hits, VType::I32, "999999").is_err());
    assert_eq!(snapshot(&mem, base, 64), before);
}

#[test]
fn overlapping_results_are_rejected_before_writing() {
    let (mut mem, base) = memory(64);
    let wide = result(&mut mem, base, VType::I32, "600");
    let narrow = SearchResult { vtype: VType::U16, ..wide };
    let before = snapshot(&mem, base, 64);
    for mut hits in [[wide, narrow], [narrow, wide], [wide, wide]] {
        assert!(set_all(&mut mem, &mut hits, VType::Auto, "1000").is_err());
        assert_eq!(snapshot(&mem, base, 64), before);
    }
}

#[test]
fn broad_search_is_blocked_not_truncated_to_a_subset() {
    let size = (MAX_BULK_WRITES as u32 + 1) * 4;
    let (mut mem, base) = memory(size);
    let mut hits: Vec<_> = (0..=MAX_BULK_WRITES)
        .map(|i| result(&mut mem, base + i as u32 * 4, VType::I32, "600"))
        .collect();
    let before = snapshot(&mem, base, size);
    assert!(set_all(&mut mem, &mut hits, VType::Auto, "1000").is_err());
    assert_eq!(snapshot(&mem, base, size), before);
    hits.pop();
    assert_eq!(set_all(&mut mem, &mut hits, VType::Auto, "1000"), Ok(MAX_BULK_WRITES));
    assert_eq!(VType::I32.read_at(&mem, base + MAX_BULK_WRITES as u32 * 4), Some(600));
}

#[test]
fn stale_and_freed_results_do_not_partially_write() {
    let (mut mem, base) = memory(64);
    let other = mem.alloc(64).to_bits();
    let mut hits = [
        result(&mut mem, base, VType::I32, "600"),
        result(&mut mem, other, VType::I32, "600"),
    ];
    assert!(VType::I32.write_at(&mut mem, other, 601));
    assert!(set_all(&mut mem, &mut hits, VType::Auto, "1000").is_err());
    assert_eq!(VType::I32.read_at(&mem, base), Some(600));
    mem.free(MutVoidPtr::from_bits(other));
    assert!(set_all(&mut mem, &mut hits, VType::Auto, "1000").is_err());
    assert_eq!(VType::I32.read_at(&mem, base), Some(600));
}

#[test]
fn unaligned_and_out_of_allocation_hits_are_blocked() {
    let (mut mem, base) = memory(64);
    let good = result(&mut mem, base, VType::I32, "600");
    let unaligned = result(&mut mem, base + 9, VType::I32, "600");
    let outside = SearchResult { addr: base + 64, ..good };
    for bad in [unaligned, outside] {
        let mut hits = [good, bad];
        let before = snapshot(&mem, base, 64);
        assert!(set_all(&mut mem, &mut hits, VType::Auto, "1000").is_err());
        assert_eq!(snapshot(&mem, base, 64), before);
    }
}

#[test]
fn valid_batch_preserves_types_neighbours_and_displayed_values() {
    let (mut mem, base) = memory(64);
    let mut hits = [
        result(&mut mem, base, VType::I32, "600"),
        result(&mut mem, base + 8, VType::U16, "600"),
        result(&mut mem, base + 16, VType::F32, "600"),
    ];
    assert_eq!(set_all(&mut mem, &mut hits, VType::Auto, "1000"), Ok(3));
    for hit in hits {
        let expected = hit.vtype.parse("1000").unwrap();
        assert_eq!(hit.bits, expected);
        assert_eq!(hit.vtype.read_at(&mem, hit.addr), Some(expected));
        assert_eq!(VType::U8.read_at(&mem, hit.addr + hit.vtype.size()), Some(0xAA));
    }
}

#[test]
fn invalid_memory_accesses_fail_without_a_panic_or_sink_write() {
    let (mut mem, _) = memory(64);
    for addr in [0, PAGE_SIZE - 1, u32::MAX - 1] {
        assert_eq!(VType::I32.read_at(&mem, addr), None);
        assert!(!VType::I32.write_at(&mut mem, addr, 123));
    }
    assert!(set_all(&mut mem, &mut [], VType::Auto, "1000").is_err());
}
