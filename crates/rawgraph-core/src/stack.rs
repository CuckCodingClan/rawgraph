//! Image groups: which images of a collection belong together (an exposure
//! bracket, a burst, a panorama sequence) and which member stands for the group.
//!
//! Pure functions on identities and capture metadata, shared by the HDR Merge
//! node and later stacking nodes.

use std::collections::HashMap;
use std::rc::Rc;

use crate::capture::CaptureInfo;
use crate::graph::{ImageId, NodeKind};

/// Largest group size any kind of group accepts (every kind has its own, smaller or equal limit,
/// see [`GroupKindSpec::max_size`]).
pub const GROUP_MAX_SIZE: usize = 60;
/// Smallest group size the settings accept.
pub const GROUP_MIN_SIZE: usize = 2;
/// Largest time gap (seconds) any kind of group accepts.
pub const GROUP_MAX_GAP_S: f32 = 120.0;
/// EVs closer than this (stops) count as the same exposure.
const SAME_EV: f32 = 0.15;
/// A bracket needs at least this many stops between its darkest and brightest frame; a series
/// with less (a burst at one exposure) stays single images.
pub const MIN_BRACKET_SPREAD_EV: f32 = 0.7;

/// Option labels of the HDR Merge rule list (the order of its [`GroupKindSpec::rules`]).
pub const GROUP_RULES: [&str; 4] = ["Capture time", "Bracket", "Manual", "Whole collection"];
/// Option labels of the Panorama rule list (no Bracket).
pub const PANORAMA_GROUP_RULES: [&str; 3] = ["Capture time", "Manual", "Whole collection"];

/// How a collection is cut into groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GroupRule {
    /// Consecutive captures at most `gap_s` apart.
    CaptureTime,
    /// Like `CaptureTime`, and a repeated exposure value starts a new group.
    Bracket,
    /// Images with the same non-zero `group` number.
    Manual,
    /// The whole collection (up to `max_size`).
    Whole,
}

/// What a node that cuts its input collection into groups (one output image per group) needs to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupKindSpec {
    /// Which member stands for the group (the output keeps its identity).
    pub reference: Reference,
    /// The rules the node offers, in the order of its "Group by" option list.
    pub rules: &'static [GroupRule],
    /// The rule a fresh node starts with.
    pub default_rule: GroupRule,
    /// Largest group size the node accepts.
    pub max_size: usize,
}

impl GroupKindSpec {
    /// The rule behind option `index` of the node's own list; an index outside the list gives the default.
    pub fn rule(&self, index: usize) -> GroupRule {
        self.rules.get(index).copied().unwrap_or(self.default_rule)
    }
}

const HDR_RULES: &[GroupRule] = &[GroupRule::CaptureTime, GroupRule::Bracket, GroupRule::Manual, GroupRule::Whole];
const PANORAMA_RULES: &[GroupRule] = &[GroupRule::CaptureTime, GroupRule::Manual, GroupRule::Whole];

/// The group description of a node kind; `None` for kinds that do not make groups (HDR Merge and
/// Panorama do).
pub fn group_kind(kind: NodeKind) -> Option<GroupKindSpec> {
    match kind {
        NodeKind::HdrMerge => Some(GroupKindSpec { reference: Reference::MiddleExposure, rules: HDR_RULES, default_rule: GroupRule::Bracket, max_size: 25 }),
        NodeKind::Panorama => Some(GroupKindSpec { reference: Reference::Middle, rules: PANORAMA_RULES, default_rule: GroupRule::CaptureTime, max_size: 25 }),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupSettings {
    pub rule: GroupRule,
    pub gap_s: f32,
    pub max_size: usize,
}

impl Default for GroupSettings {
    fn default() -> Self {
        GroupSettings { rule: GroupRule::Bracket, gap_s: 2.0, max_size: 9 }
    }
}

/// Which member represents a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reference {
    /// The member with the median exposure value (HDR Merge).
    MiddleExposure,
    /// The middle member in group order (panorama).
    Middle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageGroup {
    pub members: Vec<ImageId>,
    pub reference: ImageId,
}

/// Exposure of a frame in stops: `log2(shutter * ISO/100 / aperture^2)`. A
/// longer exposure, higher ISO or wider aperture gives a larger value. `None`
/// when shutter, ISO or aperture is missing or not positive.
pub fn exposure_ev(c: &CaptureInfo) -> Option<f32> {
    ev_parts(c).and_then(|(base, ap)| ap.map(|a| base - a))
}

/// Exposure in stops from shutter and ISO alone (`log2(shutter * ISO/100)`), plus the aperture
/// term `2 * log2(f-number)` when the f-number is known.
fn ev_parts(c: &CaptureInfo) -> Option<(f32, Option<f32>)> {
    let (iso, t) = (c.iso?, c.shutter_s?);
    if !(iso > 0.0 && t > 0.0 && iso.is_finite() && t.is_finite()) {
        return None;
    }
    let ap = c.aperture.filter(|n| *n > 0.0 && n.is_finite()).map(|n| 2.0 * n.log2());
    Some(((t * iso / 100.0).log2(), ap))
}

/// Exposure values of frames that belong together, comparable with each other: the aperture counts
/// only when every frame that has shutter and ISO also has an f-number; otherwise the frames are
/// compared by shutter x ISO alone (the aperture is taken as equal across the group). `None` for
/// a frame without capture info, shutter or ISO.
pub fn group_exposure_evs(caps: &[Option<&CaptureInfo>]) -> Vec<Option<f32>> {
    let parts: Vec<Option<(f32, Option<f32>)>> = caps.iter().map(|c| c.and_then(ev_parts)).collect();
    let with_ap = parts.iter().flatten().all(|(_, ap)| ap.is_some());
    parts.iter().map(|p| p.map(|(base, ap)| if with_ap { base - ap.unwrap_or(0.0) } else { base })).collect()
}

/// Exposure difference of two frames in stops: with aperture when both know it, else by shutter x ISO.
fn ev_pair(a: Option<(f32, Option<f32>)>, b: Option<(f32, Option<f32>)>) -> Option<f32> {
    let (a, b) = (a?, b?);
    Some(match (a.1, b.1) {
        (Some(x), Some(y)) => (a.0 - x) - (b.0 - y),
        _ => a.0 - b.0,
    })
}

type CapFn<'a> = &'a dyn Fn(ImageId) -> Option<Rc<CaptureInfo>>;

/// Cuts `input` into groups. Every input image ends up in exactly one group;
/// the groups are listed by the input position of their reference.
pub fn group_images(
    input: &[ImageId],
    cap: CapFn,
    manual: &dyn Fn(ImageId) -> Option<i64>,
    s: &GroupSettings,
    r: Reference,
) -> Vec<ImageGroup> {
    let max_size = s.max_size.clamp(GROUP_MIN_SIZE, GROUP_MAX_SIZE);
    let gap_ms = (s.gap_s.clamp(0.0, GROUP_MAX_GAP_S) as f64 * 1000.0).round() as i64;
    let mut seen = std::collections::HashSet::new();
    let input: Vec<ImageId> = input.iter().copied().filter(|i| seen.insert(*i)).collect();
    let caps: HashMap<ImageId, Option<Rc<CaptureInfo>>> = input.iter().map(|&i| (i, cap(i))).collect();
    let parts: HashMap<ImageId, Option<(f32, Option<f32>)>> =
        caps.iter().map(|(&i, c)| (i, c.as_deref().and_then(ev_parts))).collect();

    let mut raw: Vec<Vec<ImageId>> = Vec::new();
    match s.rule {
        GroupRule::Whole => {
            for (k, &i) in input.iter().enumerate() {
                if k < max_size {
                    if k == 0 {
                        raw.push(Vec::new());
                    }
                    raw[0].push(i);
                } else {
                    raw.push(vec![i]);
                }
            }
        }
        GroupRule::Manual => {
            let mut index: HashMap<i64, usize> = HashMap::new();
            for &i in &input {
                match manual(i).filter(|g| *g != 0) {
                    Some(g) => match index.get(&g) {
                        Some(&at) if raw[at].len() < max_size => raw[at].push(i),
                        Some(_) => raw.push(vec![i]),
                        None => {
                            index.insert(g, raw.len());
                            raw.push(vec![i]);
                        }
                    },
                    None => raw.push(vec![i]),
                }
            }
        }
        GroupRule::CaptureTime | GroupRule::Bracket => {
            let mut timed: Vec<(i64, ImageId)> = Vec::new();
            for &i in &input {
                match cap(i).and_then(|c| c.captured_unix_ms) {
                    Some(t) => timed.push((t, i)),
                    None => raw.push(vec![i]),
                }
            }
            timed.sort_by_key(|(t, _)| *t); // stable: ties keep input order
            let bracket = s.rule == GroupRule::Bracket;
            let mut current: Vec<ImageId> = Vec::new();
            // When the latest exposure of the current group ended (start + shutter time).
            let mut last_end = 0i64;
            for (t, i) in timed {
                let ev = parts.get(&i).copied().flatten();
                let end = t + caps.get(&i).and_then(|c| c.as_deref()).and_then(|c| c.shutter_s).filter(|s| s.is_finite() && *s > 0.0).map_or(0, |s| (s as f64 * 1000.0).round() as i64);
                if bracket && ev.is_none() {
                    // Unknown exposure: never part of a bracket.
                    if !current.is_empty() {
                        raw.push(std::mem::take(&mut current));
                    }
                    raw.push(vec![i]);
                    continue;
                }
                let split = !current.is_empty()
                    && (t - last_end > gap_ms
                        || current.len() >= max_size
                        || (bracket
                            && current.iter().any(|m| {
                                ev_pair(parts.get(m).copied().flatten(), ev).is_some_and(|d| d.abs() < SAME_EV)
                            })));
                if split {
                    raw.push(std::mem::take(&mut current));
                    last_end = 0;
                }
                current.push(i);
                last_end = last_end.max(end);
            }
            if !current.is_empty() {
                raw.push(current);
            }
        }
    }

    if s.rule == GroupRule::Bracket {
        // A series that does not spread over enough stops is no bracket: its images stay single.
        raw = raw
            .into_iter()
            .flat_map(|members| {
                let spread = ev_spread(&members, &parts);
                if members.len() > 1 && spread.is_none_or(|s| s < MIN_BRACKET_SPREAD_EV) {
                    members.into_iter().map(|m| vec![m]).collect::<Vec<_>>()
                } else {
                    vec![members]
                }
            })
            .collect();
    }
    let evs: HashMap<ImageId, Option<f32>> = {
        // Exposure values per group (aperture counted only if every member has it).
        let mut m = HashMap::new();
        for members in &raw {
            let list: Vec<Option<&CaptureInfo>> =
                members.iter().map(|i| caps.get(i).and_then(|c| c.as_deref())).collect();
            for (i, ev) in members.iter().zip(group_exposure_evs(&list)) {
                m.insert(*i, ev);
            }
        }
        m
    };
    let pos: HashMap<ImageId, usize> = input.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    let mut groups: Vec<ImageGroup> = raw
        .into_iter()
        .map(|members| {
            let reference = pick_reference(&members, &evs, &pos, r);
            ImageGroup { members, reference }
        })
        .collect();
    groups.sort_by_key(|g| pos.get(&g.reference).copied().unwrap_or(usize::MAX));
    groups
}

/// Largest exposure difference between two members (stops); `None` if one has no exposure value.
fn ev_spread(members: &[ImageId], parts: &HashMap<ImageId, Option<(f32, Option<f32>)>>) -> Option<f32> {
    let mut worst = 0.0f32;
    for (k, a) in members.iter().enumerate() {
        for b in &members[k + 1..] {
            let d = ev_pair(parts.get(a).copied().flatten(), parts.get(b).copied().flatten())?;
            worst = worst.max(d.abs());
        }
    }
    Some(worst)
}

fn pick_reference(
    members: &[ImageId],
    evs: &HashMap<ImageId, Option<f32>>,
    pos: &HashMap<ImageId, usize>,
    r: Reference,
) -> ImageId {
    let mid = (members.len() - 1) / 2;
    if r == Reference::MiddleExposure && members.iter().all(|m| evs.get(m).copied().flatten().is_some()) {
        let mut order: Vec<ImageId> = members.to_vec();
        order.sort_by(|a, b| {
            let ea = evs.get(a).copied().flatten().unwrap_or(0.0);
            let eb = evs.get(b).copied().flatten().unwrap_or(0.0);
            ea.total_cmp(&eb).then(pos.get(a).cmp(&pos.get(b)))
        });
        return order[mid];
    }
    members[mid]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> ImageId {
        ImageId(n)
    }

    /// (id, time in ms, shutter in s); ISO 100, f/1 so EV = log2(shutter).
    fn caps(list: &[(u64, Option<i64>, Option<f32>)]) -> HashMap<ImageId, Rc<CaptureInfo>> {
        list.iter()
            .map(|&(n, t, sh)| {
                let c = CaptureInfo {
                    captured_unix_ms: t,
                    shutter_s: sh,
                    iso: sh.map(|_| 100.0),
                    aperture: sh.map(|_| 1.0),
                    ..Default::default()
                };
                (id(n), Rc::new(c))
            })
            .collect()
    }

    fn run(
        list: &[(u64, Option<i64>, Option<f32>)],
        manual: &[(u64, i64)],
        rule: GroupRule,
        gap: f32,
        max: usize,
        r: Reference,
    ) -> Vec<(Vec<u64>, u64)> {
        let c = caps(list);
        let input: Vec<ImageId> = list.iter().map(|l| id(l.0)).collect();
        let m: HashMap<ImageId, i64> = manual.iter().map(|&(a, b)| (id(a), b)).collect();
        let s = GroupSettings { rule, gap_s: gap, max_size: max };
        group_images(&input, &|i| c.get(&i).cloned(), &|i| m.get(&i).copied(), &s, r)
            .into_iter()
            .map(|g| (g.members.iter().map(|i| i.0).collect(), g.reference.0))
            .collect()
    }

    const NO: &[(u64, i64)] = &[];

    #[test]
    fn exposure_ev_reference_points() {
        let c = |iso, t, n| CaptureInfo { iso: Some(iso), shutter_s: Some(t), aperture: Some(n), ..Default::default() };
        assert!(exposure_ev(&c(100.0, 1.0, 1.0)).unwrap().abs() < 1e-6);
        let a = exposure_ev(&c(100.0, 0.01, 4.0)).unwrap();
        assert!((exposure_ev(&c(100.0, 0.02, 4.0)).unwrap() - a - 1.0).abs() < 1e-5, "double time = +1 stop");
        assert!((exposure_ev(&c(200.0, 0.01, 4.0)).unwrap() - a - 1.0).abs() < 1e-5, "double ISO = +1 stop");
        assert!((exposure_ev(&c(100.0, 0.01, 2.0)).unwrap() - a - 2.0).abs() < 1e-5, "two stops wider");
        assert_eq!(exposure_ev(&CaptureInfo::default()), None);
        assert_eq!(exposure_ev(&c(100.0, 0.0, 4.0)), None);
        assert_eq!(exposure_ev(&c(100.0, f32::NAN, 4.0)), None);
    }

    #[test]
    fn group_evs_fall_back_to_shutter_and_iso_without_aperture() {
        let full = |iso, t, n| CaptureInfo { iso: Some(iso), shutter_s: Some(t), aperture: n, ..Default::default() };
        // All with aperture: the aperture counts (f/2 vs f/4 = 2 stops).
        let (a, b) = (full(100.0, 1.0, Some(2.0)), full(100.0, 1.0, Some(4.0)));
        let evs = group_exposure_evs(&[Some(&a), Some(&b)]);
        assert!((evs[0].unwrap() - evs[1].unwrap() - 2.0).abs() < 1e-5);
        // One without aperture: nobody's aperture counts, shutter x ISO decide.
        let c = full(100.0, 2.0, None);
        let evs = group_exposure_evs(&[Some(&a), Some(&c)]);
        assert!((evs[1].unwrap() - evs[0].unwrap() - 1.0).abs() < 1e-5, "{evs:?}");
        // Missing shutter / capture stays unknown.
        let d = CaptureInfo { iso: Some(100.0), ..Default::default() };
        assert_eq!(group_exposure_evs(&[Some(&d), None]), vec![None, None]);
    }

    /// (id, time in ms, shutter in s, ISO) without any aperture.
    fn no_aperture(list: &[(u64, i64, f32, f32)], rule: GroupRule, gap: f32) -> Vec<(Vec<u64>, u64)> {
        let c: HashMap<ImageId, Rc<CaptureInfo>> = list
            .iter()
            .map(|&(n, t, sh, iso)| {
                (id(n), Rc::new(CaptureInfo { captured_unix_ms: Some(t), shutter_s: Some(sh), iso: Some(iso), ..Default::default() }))
            })
            .collect();
        let input: Vec<ImageId> = list.iter().map(|l| id(l.0)).collect();
        let s = GroupSettings { rule, gap_s: gap, max_size: 9 };
        group_images(&input, &|i| c.get(&i).cloned(), &|_| None, &s, Reference::MiddleExposure)
            .into_iter()
            .map(|g| (g.members.iter().map(|i| i.0).collect(), g.reference.0))
            .collect()
    }

    #[test]
    fn a_bracket_without_aperture_is_still_a_bracket() {
        // Typical camera export without an f-number: 1/4, 1, 4 s at ISO 100, started 0.1 s apart.
        let list = [(1, 0, 0.25, 100.0), (2, 300, 1.0, 100.0), (3, 1_400, 4.0, 100.0)];
        assert_eq!(no_aperture(&list, GroupRule::Bracket, 2.0), vec![(vec![1, 2, 3], 2)]);
        // ISO brackets count as well.
        let list = [(1, 0, 0.01, 100.0), (2, 100, 0.01, 200.0), (3, 200, 0.01, 400.0)];
        assert_eq!(no_aperture(&list, GroupRule::Bracket, 2.0), vec![(vec![1, 2, 3], 2)]);
    }

    #[test]
    fn the_gap_is_measured_from_the_end_of_the_previous_exposure() {
        // Night bracket 1 s / 4 s / 15 s, each started right after the previous one ended
        // (0.3 s of write time): start-to-start the gaps are 1.3 s, 4.3 s.
        let list = [(1, 0, 1.0, 100.0), (2, 1_300, 4.0, 100.0), (3, 5_600, 15.0, 100.0)];
        assert_eq!(no_aperture(&list, GroupRule::Bracket, 2.0), vec![(vec![1, 2, 3], 2)]);
        // A real pause after the end still separates them.
        let list = [(1, 0, 1.0, 100.0), (2, 1_300, 4.0, 100.0), (3, 8_400, 15.0, 100.0)];
        assert_eq!(no_aperture(&list, GroupRule::Bracket, 2.0), vec![(vec![1, 2], 1), (vec![3], 3)]);
        // Same for plain capture time.
        assert_eq!(no_aperture(&list, GroupRule::CaptureTime, 2.0).len(), 2);
    }

    #[test]
    fn a_bracket_needs_a_minimum_exposure_spread() {
        // Burst at one exposure with small variations (0.5 stop in total): single images.
        let list = [(1, 0, 0.01, 100.0), (2, 100, 0.0126, 100.0), (3, 200, 0.0142, 100.0)];
        let g = no_aperture(&list, GroupRule::Bracket, 2.0);
        assert_eq!(g, vec![(vec![1], 1), (vec![2], 2), (vec![3], 3)]);
        // 0.75 stop in total is enough.
        let list = [(1, 0, 0.01, 100.0), (2, 100, 0.0126, 100.0), (3, 200, 0.0168, 100.0)];
        assert_eq!(no_aperture(&list, GroupRule::Bracket, 2.0), vec![(vec![1, 2, 3], 2)]);
        // The other rules are not affected.
        let list = [(1, 0, 0.01, 100.0), (2, 100, 0.01, 100.0)];
        assert_eq!(no_aperture(&list, GroupRule::CaptureTime, 2.0), vec![(vec![1, 2], 1)]);
    }

    #[test]
    fn capture_time_groups_by_gap_and_keeps_input_order_of_references() {
        // Input order differs from capture order; 1.0 s apart inside, 10 s between groups.
        let list = [(1, Some(10_000), None), (2, Some(0), None), (3, Some(1_000), None), (4, Some(11_000), None)];
        let g = run(&list, NO, GroupRule::CaptureTime, 2.0, 9, Reference::Middle);
        assert_eq!(g, vec![(vec![1, 4], 1), (vec![2, 3], 2)]);
        // Every image is in exactly one group.
        let all: Vec<u64> = g.iter().flat_map(|(m, _)| m.clone()).collect();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn gap_boundary_is_inclusive() {
        let list = [(1, Some(0), None), (2, Some(2_000), None), (3, Some(4_001), None)];
        let g = run(&list, NO, GroupRule::CaptureTime, 2.0, 9, Reference::Middle);
        assert_eq!(g, vec![(vec![1, 2], 1), (vec![3], 3)]);
    }

    #[test]
    fn unknown_capture_time_is_a_singleton_except_whole_and_manual() {
        let list = [(1, Some(0), None), (2, None, None), (3, Some(500), None)];
        let g = run(&list, NO, GroupRule::CaptureTime, 2.0, 9, Reference::Middle);
        assert_eq!(g, vec![(vec![1, 3], 1), (vec![2], 2)]);
        let w = run(&list, NO, GroupRule::Whole, 2.0, 9, Reference::Middle);
        assert_eq!(w, vec![(vec![1, 2, 3], 2)]);
    }

    #[test]
    fn max_size_caps_a_time_group() {
        let list: Vec<_> = (1..=7).map(|i| (i, Some(i as i64 * 100), None)).collect();
        let g = run(&list, NO, GroupRule::CaptureTime, 2.0, 3, Reference::Middle);
        assert_eq!(g.iter().map(|(m, _)| m.len()).collect::<Vec<_>>(), vec![3, 3, 1]);
        // Sizes below the minimum are lifted to 2.
        let g = run(&list, NO, GroupRule::CaptureTime, 2.0, 1, Reference::Middle);
        assert_eq!(g.iter().map(|(m, _)| m.len()).collect::<Vec<_>>(), vec![2, 2, 2, 1]);
    }

    #[test]
    fn bracket_starts_a_new_group_when_an_exposure_repeats() {
        // EV via shutter: 1/4, 1, 4 (-2, 0, +2), then the series repeats.
        let (a, b, c) = (Some(0.25), Some(1.0), Some(4.0));
        let list = [
            (1, Some(0), a),
            (2, Some(500), b),
            (3, Some(1_000), c),
            (4, Some(1_500), a),
            (5, Some(2_000), b),
            (6, Some(2_500), c),
        ];
        let g = run(&list, NO, GroupRule::Bracket, 2.0, 9, Reference::MiddleExposure);
        assert_eq!(g, vec![(vec![1, 2, 3], 2), (vec![4, 5, 6], 5)]);
        // Plain capture time would have merged all six.
        assert_eq!(run(&list, NO, GroupRule::CaptureTime, 2.0, 9, Reference::Middle).len(), 1);
    }

    #[test]
    fn bracket_with_unknown_ev_makes_singletons_and_breaks_the_series() {
        let list = [
            (1, Some(0), Some(0.25)),
            (2, Some(500), None),
            (3, Some(1_000), Some(1.0)),
            (4, Some(1_500), Some(4.0)),
        ];
        let g = run(&list, NO, GroupRule::Bracket, 2.0, 9, Reference::MiddleExposure);
        assert_eq!(g, vec![(vec![1], 1), (vec![2], 2), (vec![3, 4], 3)]);
    }

    #[test]
    fn near_equal_ev_counts_as_repeat_but_a_third_of_a_stop_does_not() {
        // 1/3 stop apart (0.333) is a different exposure; 0.1 stop apart is a repeat.
        let third = 2f32.powf(1.0 / 3.0);
        let tenth = 2f32.powf(0.1);
        let list = [(1, Some(0), Some(1.0)), (2, Some(100), Some(third)), (3, Some(200), Some(2.0)), (4, Some(300), Some(tenth))];
        let g = run(&list, NO, GroupRule::Bracket, 2.0, 9, Reference::MiddleExposure);
        assert_eq!(g.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(), vec![vec![1, 2, 3], vec![4]]);
    }

    #[test]
    fn manual_groups_by_number_with_zero_as_singletons_and_caps() {
        let list: Vec<_> = (1..=7).map(|i| (i, None, None)).collect();
        let manual = [(1, 5), (2, 0), (3, 5), (4, 7), (5, 5), (6, 5), (7, 5)];
        let g = run(&list, &manual, GroupRule::Manual, 2.0, 3, Reference::Middle);
        // Group 5 holds 1,3,5 (cap 3); 6 and 7 overflow into singletons; 2 has no group; 4 is alone in 7.
        assert_eq!(g, vec![(vec![2], 2), (vec![1, 3, 5], 3), (vec![4], 4), (vec![6], 6), (vec![7], 7)]);
    }

    #[test]
    fn whole_collection_is_capped_and_the_rest_are_singletons() {
        let list: Vec<_> = (1..=6).map(|i| (i, None, None)).collect();
        let g = run(&list, NO, GroupRule::Whole, 2.0, 4, Reference::Middle);
        assert_eq!(g, vec![(vec![1, 2, 3, 4], 2), (vec![5], 5), (vec![6], 6)]);
        assert!(run(&[], NO, GroupRule::Whole, 2.0, 4, Reference::Middle).is_empty());
    }

    #[test]
    fn reference_is_the_median_exposure_lower_middle_for_even_counts() {
        // Capture order 1..4 with EVs 3, 0, 2, -1: sorted -1(4) 0(2) 2(3) 3(1) -> index (4-1)/2 = 1 -> image 2.
        let list = [
            (1, Some(0), Some(8.0)),
            (2, Some(100), Some(1.0)),
            (3, Some(200), Some(4.0)),
            (4, Some(300), Some(0.5)),
        ];
        let g = run(&list, NO, GroupRule::CaptureTime, 2.0, 9, Reference::MiddleExposure);
        assert_eq!(g, vec![(vec![1, 2, 3, 4], 2)]);
        // Odd count: the true median.
        let g = run(&list[..3], NO, GroupRule::CaptureTime, 2.0, 9, Reference::MiddleExposure);
        assert_eq!(g, vec![(vec![1, 2, 3], 3)]);
    }

    #[test]
    fn unknown_ev_falls_back_to_the_middle_in_capture_order() {
        let list = [(1, Some(0), Some(8.0)), (2, Some(100), None), (3, Some(200), Some(4.0)), (4, Some(300), None)];
        let g = run(&list, NO, GroupRule::CaptureTime, 2.0, 9, Reference::MiddleExposure);
        assert_eq!(g, vec![(vec![1, 2, 3, 4], 2)]);
        let m = run(&list, NO, GroupRule::CaptureTime, 2.0, 9, Reference::Middle);
        assert_eq!(m, g);
    }

    #[test]
    fn equal_ev_ties_keep_input_order() {
        // Whole rule, all the same EV: median index of 3 is 1 -> second in input order.
        let list = [(5, None, Some(1.0)), (3, None, Some(1.0)), (9, None, Some(1.0))];
        let g = run(&list, NO, GroupRule::Whole, 2.0, 9, Reference::MiddleExposure);
        assert_eq!(g, vec![(vec![5, 3, 9], 3)]);
    }

    #[test]
    fn duplicates_in_the_input_are_ignored_and_output_is_ordered_by_reference() {
        let list = [(1, Some(10_000), None), (2, Some(0), None)];
        let c = caps(&list);
        let input = [id(1), id(2), id(1)];
        let s = GroupSettings { rule: GroupRule::CaptureTime, ..Default::default() };
        let g = group_images(&input, &|i| c.get(&i).cloned(), &|_| None, &s, Reference::Middle);
        assert_eq!(g.iter().map(|g| g.reference.0).collect::<Vec<_>>(), vec![1, 2]);
    }
}
