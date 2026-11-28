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
}

impl BlockReport {
    /// Anything the user should look at before running.
    pub fn has_warnings(&self) -> bool {
        !self.missing_numbers.is_empty() || !self.duplicate_numbers.is_empty() || !self.unknown_numbers.is_empty() || !self.no_number.is_empty()
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
    }
    for b in sheet.blocks() {
        let inside: Vec<(&u64, &Vec<usize>)> = by_number.range(b.first_no..=b.last_no).collect();
        report.blocks.push(BlockView {
            caption: b.caption,
            first_no: b.first_no,
            last_no: b.last_no,
            in_folder: inside.iter().map(|(_, v)| v.len()).sum(),
            first: inside.first().map(|(_, v)| v[0]),
            last: inside.last().map(|(_, v)| v[0]),
        });
    }
    report
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
        // 3 is missing, 2 exists twice (two different shots), 9 is not in the sheet, "x" has no number.
        let (_t, plan) = plan_of(&["J-1.jpg", "J-2.jpg", "K-2.jpg", "J-4.jpg", "J-9.jpg", "x.jpg"]);
        let r = check_blocks(&plan, &sheet());
        assert_eq!(r.missing_numbers, vec![3, 5, 6], "the range is the folder's (1..9)");
        assert_eq!(r.duplicate_numbers.len(), 1);
        assert_eq!(r.duplicate_numbers[0].0, 2);
        assert_eq!(r.unknown_numbers.iter().map(|u| u.0).collect::<Vec<_>>(), vec![9]);
        assert_eq!(r.no_number.len(), 1);
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
    fn a_folder_that_starts_late_does_not_report_the_sheets_head_as_missing() {
        let (_t, plan) = plan_of(&["J-4.jpg", "J-5.jpg"]);
        let r = check_blocks(&plan, &sheet());
        assert!(r.missing_numbers.is_empty());
        assert_eq!(r.blocks[0].in_folder, 0);
        assert!(r.blocks[0].first.is_none());
    }
}
