//! The block-boundary check for sheet-based renames.
//!
//! The captions of a block stay the same from the first to the last photo of a person or team, so a
//! shift by one number inside a block is invisible in the captions: only the number reveals it. This
//! lists, per block of the sheet, the first and the last file of the folder that falls into it, and
//! the numbers that are missing, doubled or unknown.

use std::collections::{BTreeMap, BTreeSet};

use crate::plan::{EntryKind, Plan};
use crate::sheet::CaptionSheet;

/// One block of the sheet and where it lands in the folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockView {
    pub caption: String,
    pub first_no: u64,
    pub last_no: u64,
    /// Files of the plan whose number is inside the block.
    pub in_folder: usize,
    /// Index into `Plan::entries` of the file with the lowest / highest number inside the block.
    pub first: Option<usize>,
    pub last: Option<usize>,
}

impl BlockView {
    /// Photos the sheet expects in this block.
    pub fn expected(&self) -> u64 {
        self.last_no - self.first_no + 1
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockReport {
    pub blocks: Vec<BlockView>,
    /// Numbers of the sheet between the lowest and the highest number of the folder with no file.
    pub missing_numbers: Vec<u64>,
    /// Numbers carried by several different files (RAW + JPEG of one shot do not count): the entries.
    pub duplicate_numbers: Vec<(u64, Vec<usize>)>,
    /// Files whose number is not in the sheet.
    pub unknown_numbers: Vec<(u64, usize)>,
    /// Files without a number in the name.
    pub no_number: Vec<usize>,
    /// Numbers of the sheet below the lowest or above the highest number of the folder (a missing head or tail).
    pub outside_range: Vec<u64>,
    /// Everything the user must look at before renaming, in plain sentences (also shown in the main window).
    pub warnings: Vec<String>,
}

impl BlockReport {
    /// Anything the user should look at before running.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
            || !self.outside_range.is_empty()
            || !self.missing_numbers.is_empty() || !self.duplicate_numbers.is_empty() || !self.unknown_numbers.is_empty() || !self.no_number.is_empty()
    }
}

/// Builds the report from the main entries of `plan` (their `number` is what the sheet is matched with).
pub fn check_blocks(plan: &Plan, sheet: &CaptionSheet) -> BlockReport {
    let mut by_number: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    let mut report = BlockReport::default();
    for (i, e) in plan.entries.iter().enumerate().filter(|(_, e)| e.kind == EntryKind::Main) {
        match e.number {
            Some(n) => by_number.entry(n).or_default().push(i),
            None => report.no_number.push(i),
        }
    }
    let known: BTreeSet<u64> = sheet.numbers().into_iter().collect();
    for (&n, idx) in &by_number {
        // Same folder and same stem = one shot in several formats, not a duplicate.
        let mut shots: Vec<(String, String)> = idx.iter().map(|&i| shot_id(plan, i)).collect();
        shots.sort();
        shots.dedup();
        if shots.len() > 1 {
            report.duplicate_numbers.push((n, idx.clone()));
        }
        if !known.contains(&n) {
            report.unknown_numbers.push((n, idx[0]));
        }
    }
    if let (Some(&lo), Some(&hi)) = (by_number.keys().next(), by_number.keys().next_back()) {
        report.missing_numbers = known.range(lo..=hi).copied().filter(|n| !by_number.contains_key(n)).collect();
        report.outside_range = known.iter().copied().filter(|n| (*n < lo || *n > hi) && !by_number.contains_key(n)).collect();
    }
    for b in sheet.blocks() {
        let inside: Vec<(&u64, &Vec<usize>)> = by_number.range(b.first_no..=b.last_no).collect();
        report.blocks.push(BlockView {
            caption: b.caption,
            first_no: b.first_no,
            last_no: b.last_no,
            in_folder: inside.iter().map(|(_, v)| v.iter().map(|&i| shot_id(plan, i)).collect::<BTreeSet<_>>().len()).sum(),
            first: inside.first().map(|(_, v)| v[0]),
            last: inside.last().map(|(_, v)| v[0]),
        });
    }
    report.warnings = warnings(plan, sheet, &report, &by_number);
    report
}

fn list(title: &str, items: &[u64]) -> String {
    let shown: Vec<String> = items.iter().take(12).map(u64::to_string).collect();
    let more = if items.len() > 12 { format!(" ... and {} more", items.len() - 12) } else { String::new() };
    format!("{title}: {}{more}", shown.join(", "))
}

/// The sentences for the main window: the checks that would have caught a shifted rename.
fn warnings(plan: &Plan, sheet: &CaptionSheet, r: &BlockReport, _by_number: &BTreeMap<u64, Vec<usize>>) -> Vec<String> {
    let mut w = Vec::new();
    let shots: BTreeSet<(String, String)> = plan.entries.iter().enumerate().filter(|(_, e)| e.kind == EntryKind::Main).map(|(i, _)| shot_id(plan, i)).collect();
    if shots.len() != sheet.len() {
        w.push(format!("The folder has {} photo(s) but the sheet has {} row(s).", shots.len(), sheet.len()));
    }
    if !r.outside_range.is_empty() {
        w.push(list("Numbers of the sheet outside the range of the folder have no file (a missing head or tail?)", &r.outside_range));
    }
    if !r.missing_numbers.is_empty() {
        w.push(list("Numbers of the sheet inside the range have no file", &r.missing_numbers));
    }
    if !r.duplicate_numbers.is_empty() {
        w.push(list("Numbers carried by several different files", &r.duplicate_numbers.iter().map(|d| d.0).collect::<Vec<_>>()));
    }
    if !r.unknown_numbers.is_empty() {
        w.push(list("Files whose number is not in the sheet", &r.unknown_numbers.iter().map(|d| d.0).collect::<Vec<_>>()));
    }
    if !r.no_number.is_empty() {
        w.push(format!("{} file(s) have no usable photo number in the name.", r.no_number.len()));
    }
    let off: Vec<&BlockView> = r.blocks.iter().filter(|b| b.in_folder as u64 != b.expected()).collect();
    if !off.is_empty() {
        let b = off[0];
        w.push(format!("{} block(s) do not have the number of photos the sheet expects (first: \"{}\", No. {}-{}: {} of {}).", off.len(), b.caption, b.first_no, b.last_no, b.in_folder, b.expected()));
    }
    let offset = sheet.offset_rows();
    if !offset.is_empty() {
        let (row, n, lead) = offset[0];
        w.push(format!("{} sheet row(s) have a number in the name column that differs from the number column (row {row}: {n} against {lead}); the sheet itself may be shifted.", offset.len()));
    }
    // Capture time must grow with the photo number.
    let mut timed: Vec<(u64, i64)> = plan.entries.iter().filter(|e| e.kind == EntryKind::Main).filter_map(|e| Some((e.number?, e.capture_ms?))).collect();
    timed.sort();
    let inversions = timed.windows(2).filter(|p| p[1].1 < p[0].1).count();
    if inversions > 0 {
        w.push(format!("{inversions} photo(s) were taken before the photo with the next lower number; the numbering may not follow the shooting order."));
    }
    w
}

fn shot_id(plan: &Plan, i: usize) -> (String, String) {
    let e = &plan.entries[i];
    let dir = e.old.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = e.old.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    (dir, stem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::{src, touch};
    use crate::plan::{build, PlanOptions};
    use crate::rules::RenameRules;
    use crate::sheet::SheetColumns;

    fn sheet() -> CaptionSheet {
        let rows: Vec<Vec<String>> = [(1, "A"), (2, "A"), (3, "A"), (4, "B"), (5, "B"), (6, "C")].iter().map(|(n, c)| vec![n.to_string(), format!("{n:03}_{c}"), c.to_string()]).collect();
        CaptionSheet::from_cells(&rows, &SheetColumns::default()).unwrap()
    }

    fn plan_of(names: &[&str]) -> (tempfile::TempDir, Plan) {
        let t = tempfile::tempdir().unwrap();
        let files: Vec<_> = names.iter().enumerate().map(|(i, n)| src(&touch(t.path(), n, "x"), i as u64)).collect();
        let s = sheet();
        let plan = build(&files, &RenameRules { template: "{sheet_name}".into(), ..Default::default() }, Some(&s), &PlanOptions::default()).unwrap();
        (t, plan)
    }

    #[test]
    fn first_and_last_of_every_block() {
        let (_t, plan) = plan_of(&["J-1.jpg", "J-2.jpg", "J-3.jpg", "J-4.jpg", "J-5.jpg", "J-6.jpg"]);
        let r = check_blocks(&plan, &sheet());
        let name = |i: Option<usize>| i.map(|i| plan.entries[i].new_name()).unwrap_or_default();
        let flat: Vec<(String, usize, String, String)> = r.blocks.iter().map(|b| (b.caption.clone(), b.in_folder, name(b.first), name(b.last))).collect();
        let want = |c: &str, n: usize, a: &str, b: &str| (c.to_owned(), n, a.to_owned(), b.to_owned());
        assert_eq!(flat, vec![want("A", 3, "001_A.jpg", "003_A.jpg"), want("B", 2, "004_B.jpg", "005_B.jpg"), want("C", 1, "006_C.jpg", "006_C.jpg")]);
        assert!(!r.has_warnings());
    }

    #[test]
    fn gaps_duplicates_and_unknown_numbers_are_reported() {
        // 3 is missing, 2 exists twice (two different shots), 9 is not in the sheet.
        let (_t, plan) = plan_of(&["J-1.jpg", "J-2.jpg", "J-02.jpg", "J-4.jpg", "J-9.jpg"]);
        let r = check_blocks(&plan, &sheet());
        assert_eq!(r.missing_numbers, vec![3, 5, 6], "the range is the folder's (1..9)");
        assert_eq!(r.duplicate_numbers.len(), 1);
        assert_eq!(r.duplicate_numbers[0].0, 2);
        assert_eq!(r.unknown_numbers.iter().map(|u| u.0).collect::<Vec<_>>(), vec![9]);
        assert!(r.no_number.is_empty());
        assert!(r.has_warnings());
        // block A: numbers 1,2,2 present -> 3 files, first = 1, last = a number 2 file
        assert_eq!(r.blocks[0].in_folder, 3);
        assert_eq!(plan.entries[r.blocks[0].last.unwrap()].number, Some(2));
    }

    #[test]
    fn raw_plus_jpeg_of_one_shot_is_not_a_duplicate() {
        let (_t, plan) = plan_of(&["J-1.CR2", "J-1.jpg", "J-2.CR2"]);
        let r = check_blocks(&plan, &sheet());
        assert!(r.duplicate_numbers.is_empty());
    }

    #[test]
    fn a_missing_tail_is_a_warning_in_the_main_window() {
        // The past incident: sheet 1..6, folder 1..5 (the last photos are missing): every folder number has a row,
        // no gap inside the range, but the counts differ and the last block is short.
        let (_t, plan) = plan_of(&["J-1.jpg", "J-2.jpg", "J-3.jpg", "J-4.jpg", "J-5.jpg"]);
        let r = check_blocks(&plan, &sheet());
        assert_eq!(r.outside_range, vec![6]);
        assert!(r.has_warnings());
        let all = r.warnings.join(" | ");
        assert!(all.contains("5 photo(s) but the sheet has 6"), "{all}");
        assert!(all.contains("outside the range") && all.contains('6'), "{all}");
        assert!(all.contains("do not have the number of photos"), "{all}");
        // a clean folder has no warning at all
        let (_t, plan) = plan_of(&["J-1.jpg", "J-2.jpg", "J-3.jpg", "J-4.jpg", "J-5.jpg", "J-6.jpg"]);
        assert!(check_blocks(&plan, &sheet()).warnings.is_empty());
    }

    #[test]
    fn an_offset_inside_the_sheet_and_a_capture_time_that_runs_backwards_are_warned_about() {
        // Column B says 003 in the row of photo 2.
        let rows: Vec<Vec<String>> = [(1, "001_A"), (2, "003_A"), (3, "003_A")].iter().map(|(n, name)| vec![n.to_string(), name.to_string(), "A".to_owned()]).collect();
        let shifted = CaptionSheet::from_cells(&rows, &SheetColumns::default()).unwrap();
        assert_eq!(shifted.offset_rows(), vec![(2, 2, 3)]);
        let t = tempfile::tempdir().unwrap();
        let mut files: Vec<_> = (1..=3u64).map(|n| src(&touch(t.path(), &format!("J-{n}.jpg"), "x"), n)).collect();
        files[0].capture_unix_ms = Some(3000);
        files[1].capture_unix_ms = Some(2000);
        files[2].capture_unix_ms = Some(4000);
        let plan = build(&files, &RenameRules { template: "{sheet_name}".into(), ..Default::default() }, Some(&shifted), &PlanOptions::default()).unwrap();
        let w = check_blocks(&plan, &shifted).warnings.join(" | ");
        assert!(w.contains("differs from the number column") && w.contains("row 2"), "{w}");
        assert!(w.contains("1 photo(s) were taken before"), "{w}");
    }

    #[test]
    fn a_folder_that_starts_late_does_not_report_the_sheets_head_as_missing() {
        let (_t, plan) = plan_of(&["J-4.jpg", "J-5.jpg"]);
        let r = check_blocks(&plan, &sheet());
        assert!(r.missing_numbers.is_empty());
        assert_eq!(r.blocks[0].in_folder, 0);
        assert!(r.blocks[0].first.is_none());
    }
}
