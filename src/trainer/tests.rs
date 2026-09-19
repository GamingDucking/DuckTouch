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
fn preview_skips_overflow_without_writing_anything() {
    let (mut mem, base) = memory(64);
    let mut hits = [
        result(&mut mem, base, VType::I32, "100"),
        result(&mut mem, base + 8, VType::U8, "100"),
    ];
    let before = snapshot(&mem, base, 64);
    let plan = plan_bulk(&mem, &hits, VType::Auto, "999999", true).unwrap();
    assert_eq!((plan.writes.len(), plan.skipped), (1, 1));
    assert_eq!(snapshot(&mem, base, 64), before);
    assert_eq!(hits[0].bits, 100);
    assert_eq!(apply_bulk(&mut mem, &mut hits, &plan), Ok(1));
    assert_eq!(VType::I32.read_at(&mem, base), Some(999999));
    assert_eq!(snapshot(&mem, base + 4, 60), before[4..]);
}

#[test]
fn changing_ui_type_cannot_widen_a_bulk_write() {
    let (mut mem, base) = memory(64);
    let hits = [result(&mut mem, base, VType::U8, "100")];
    let before = snapshot(&mem, base, 64);
    assert!(plan_bulk(&mem, &hits, VType::I32, "999999", true).is_err());
    assert_eq!(snapshot(&mem, base, 64), before);
}

#[test]
fn overlapping_results_are_all_skipped_not_arbitrarily_selected() {
    let (mut mem, base) = memory(64);
    let wide = result(&mut mem, base, VType::I32, "600");
    let narrow = SearchResult { vtype: VType::U16, ..wide };
    let good = result(&mut mem, base + 8, VType::I32, "600");
    let before = snapshot(&mem, base, 64);
    for hits in [[wide, narrow, good], [narrow, wide, good], [wide, wide, good]] {
        let plan = plan_bulk(&mem, &hits, VType::Auto, "1000", true).unwrap();
        assert_eq!((plan.writes.len(), plan.skipped), (1, 2));
        assert_eq!(snapshot(&mem, base, 64), before);
    }
    let mut hits = [wide, narrow, good];
    let plan = plan_bulk(&mem, &hits, VType::Auto, "1000", true).unwrap();
    assert_eq!(apply_bulk(&mut mem, &mut hits, &plan), Ok(1));
    assert_eq!(VType::I32.read_at(&mem, base), Some(600));
    assert_eq!(VType::I32.read_at(&mem, base + 8), Some(1000));
}

#[test]
fn more_than_32_matches_are_previewed_and_written_without_truncation() {
    const COUNT: usize = 1325;
    let size = (COUNT * 4) as u32;
    let (mut mem, base) = memory(size);
    let mut hits: Vec<_> = (0..COUNT)
        .map(|i| result(&mut mem, base + i as u32 * 4, VType::I32, "135"))
        .collect();
    let before = snapshot(&mem, base, size);
    let plan = plan_bulk(&mem, &hits, VType::Auto, "999", true).unwrap();
    assert_eq!((plan.writes.len(), plan.skipped), (COUNT, 0));
    assert_eq!(snapshot(&mem, base, size), before);
    assert_eq!(apply_bulk(&mut mem, &mut hits, &plan), Ok(COUNT));
    assert!(hits.iter().all(|hit| VType::I32.read_at(&mem, hit.addr) == Some(999)));
}

#[test]
fn changed_or_freed_memory_after_preview_prevents_every_write() {
    let (mut mem, base) = memory(64);
    let other = mem.alloc(64).to_bits();
    let mut hits = [
        result(&mut mem, base, VType::I32, "600"),
        result(&mut mem, other, VType::I32, "600"),
    ];
    let plan = plan_bulk(&mem, &hits, VType::Auto, "1000", true).unwrap();
    assert!(VType::I32.write_at(&mut mem, other, 601));
    assert!(apply_bulk(&mut mem, &mut hits, &plan).is_err());
    assert_eq!(VType::I32.read_at(&mem, base), Some(600));
    assert!(VType::I32.write_at(&mut mem, other, 600));
    mem.free(MutVoidPtr::from_bits(other));
    assert!(apply_bulk(&mut mem, &mut hits, &plan).is_err());
    assert_eq!(VType::I32.read_at(&mem, base), Some(600));
}

#[test]
fn ineligible_hits_are_filtered_and_counted() {
    let (mut mem, base) = memory(64);
    let good = result(&mut mem, base, VType::I32, "600");
    let unaligned = result(&mut mem, base + 9, VType::I32, "600");
    let outside = SearchResult { addr: base + 64, ..good };
    let stale = result(&mut mem, base + 16, VType::I32, "600");
    assert!(VType::I32.write_at(&mut mem, stale.addr, 601));
    let unchanged = result(&mut mem, base + 24, VType::I32, "1000");
    let mut hits = [good, unaligned, outside, stale, unchanged];
    let before = snapshot(&mem, base, 64);
    let plan = plan_bulk(&mem, &hits, VType::Auto, "1000", true).unwrap();
    assert_eq!((plan.writes.len(), plan.skipped), (1, 4));
    assert_eq!(snapshot(&mem, base, 64), before);
    assert_eq!(apply_bulk(&mut mem, &mut hits, &plan), Ok(1));
    assert_eq!(snapshot(&mem, base + 4, 60), before[4..]);
}

#[test]
fn valid_batch_preserves_types_neighbours_and_displayed_values() {
    let (mut mem, base) = memory(64);
    let mut hits = [
        result(&mut mem, base, VType::I32, "600"),
        result(&mut mem, base + 8, VType::U16, "600"),
        result(&mut mem, base + 16, VType::F32, "600"),
    ];
    let plan = plan_bulk(&mem, &hits, VType::Auto, "1000", true).unwrap();
    assert_eq!(apply_bulk(&mut mem, &mut hits, &plan), Ok(3));
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
    assert!(plan_bulk(&mem, &[], VType::Auto, "1000", true).is_err());
}

fn bulk_command(text: &str, confirm: bool) -> TrainerCmd {
    TrainerCmd::SetAll { vtype: VType::I32, text: text.to_string(), confirm, safe_mode: true }
}

#[test]
fn set_all_requires_an_explicit_confirmation_of_the_preview() {
    let (mut mem, base) = memory(64);
    let mut trainer = Trainer::new(true);
    trainer.state.results = vec![result(&mut mem, base, VType::I32, "135")];
    trainer.handle_command(&mut mem, bulk_command("999", false));
    assert!(trainer.state.pending_bulk.is_some());
    assert_eq!(VType::I32.read_at(&mem, base), Some(135));
    // Two rapid clicks queued before the UI shows CONFIRM are only previews.
    trainer.handle_command(&mut mem, bulk_command("999", false));
    assert_eq!(VType::I32.read_at(&mem, base), Some(135));
    trainer.handle_command(&mut mem, bulk_command("999", true));
    assert_eq!(VType::I32.read_at(&mem, base), Some(999));
    assert!(trainer.state.pending_bulk.is_none());
}

#[test]
fn changed_values_or_input_require_a_new_confirmation() {
    let (mut mem, base) = memory(64);
    let mut trainer = Trainer::new(true);
    trainer.state.results = vec![
        result(&mut mem, base, VType::I32, "135"),
        result(&mut mem, base + 8, VType::I32, "135"),
    ];
    trainer.handle_command(&mut mem, bulk_command("999", false));
    assert!(VType::I32.write_at(&mut mem, base + 8, 136));
    trainer.handle_command(&mut mem, bulk_command("999", true));
    assert_eq!(VType::I32.read_at(&mem, base), Some(135));
    let plan = &trainer.state.pending_bulk.as_ref().unwrap().plan;
    assert_eq!((plan.writes.len(), plan.skipped), (1, 1));
    trainer.handle_command(&mut mem, bulk_command("1000", true));
    assert_eq!(VType::I32.read_at(&mem, base), Some(135));
    trainer.handle_command(&mut mem, bulk_command("1000", true));
    assert_eq!(VType::I32.read_at(&mem, base), Some(1000));
    assert_eq!(VType::I32.read_at(&mem, base + 8), Some(136));
}

#[test]
fn cancelled_or_expired_confirmation_cannot_write() {
    let (mut mem, base) = memory(64);
    let mut trainer = Trainer::new(true);
    trainer.state.results = vec![result(&mut mem, base, VType::I32, "135")];
    trainer.handle_command(&mut mem, bulk_command("999", false));
    trainer.handle_command(&mut mem, TrainerCmd::CancelBulk);
    assert!(trainer.state.pending_bulk.is_none());
    trainer.handle_command(&mut mem, bulk_command("999", true));
    assert_eq!(VType::I32.read_at(&mem, base), Some(135));
    trainer.state.pending_bulk.as_mut().unwrap().created_at =
        Instant::now() - BULK_CONFIRM_TIMEOUT;
    trainer.handle_command(&mut mem, bulk_command("999", true));
    assert_eq!(VType::I32.read_at(&mem, base), Some(135));
    assert!(trainer.state.pending_bulk.is_some());
}


#[test]
fn safe_mode_filters_unaligned_hits_while_normal_mode_includes_them() {
    let (mut mem, base) = memory(64);
    let mut hits = [
        result(&mut mem, base, VType::I32, "135"),
        result(&mut mem, base + 9, VType::I32, "135"),
    ];
    let safe = plan_bulk(&mem, &hits, VType::I32, "999", true).unwrap();
    assert_eq!((safe.writes.len(), safe.skipped), (1, 1));
    let normal = plan_bulk(&mem, &hits, VType::I32, "999", false).unwrap();
    assert_eq!((normal.writes.len(), normal.skipped), (2, 0));
    assert_eq!(VType::I32.read_at(&mem, base + 9), Some(135));
    assert_eq!(apply_bulk(&mut mem, &mut hits, &normal), Ok(2));
    assert_eq!(VType::I32.read_at(&mem, base + 9), Some(999));
}

#[test]
fn mode_change_cannot_confirm_another_modes_preview() {
    let (mut mem, base) = memory(64);
    let mut trainer = Trainer::new(true);
    trainer.state.results = vec![result(&mut mem, base, VType::I32, "135")];
    trainer.handle_command(&mut mem, bulk_command("999", false));
    let normal_confirm = TrainerCmd::SetAll {
        vtype: VType::I32, text: "999".to_string(), confirm: true, safe_mode: false,
    };
    trainer.handle_command(&mut mem, normal_confirm.clone());
    assert_eq!(VType::I32.read_at(&mem, base), Some(135));
    trainer.handle_command(&mut mem, normal_confirm);
    assert_eq!(VType::I32.read_at(&mem, base), Some(999));
}

#[test]
fn normal_mode_still_rejects_invalid_types_and_expired_addresses() {
    let (mut mem, base) = memory(64);
    let good = result(&mut mem, base, VType::I32, "135");
    let narrow = result(&mut mem, base + 8, VType::U8, "135");
    let invalid = SearchResult { addr: base + 64, ..good };
    let before = snapshot(&mem, base, 64);
    assert!(plan_bulk(&mem, &[good, narrow], VType::Auto, "999", false).is_err());
    assert!(plan_bulk(&mem, &[good, invalid], VType::I32, "999", false).is_err());
    assert_eq!(snapshot(&mem, base, 64), before);
}

#[test]
fn normal_mode_reports_actual_contents_after_overlapping_writes() {
    let (mut mem, base) = memory(64);
    let wide = result(&mut mem, base, VType::I32, "135");
    let narrow = SearchResult {
        addr: base + 2, vtype: VType::U16, bits: 0, changed: false,
    };
    let mut hits = [wide, narrow];
    assert!(plan_bulk(&mem, &hits, VType::Auto, "999", true).is_err());
    let plan = plan_bulk(&mem, &hits, VType::Auto, "999", false).unwrap();
    assert_eq!(apply_bulk(&mut mem, &mut hits, &plan), Ok(2));
    assert_eq!(hits[0].bits, (999 << 16) | 999);
    assert_eq!(hits[1].bits, 999);
    for hit in hits {
        assert_eq!(hit.vtype.read_at(&mem, hit.addr), Some(hit.bits));
    }
}
