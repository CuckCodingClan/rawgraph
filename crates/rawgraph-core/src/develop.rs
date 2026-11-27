//! Resolution of a Develop node's parameters against one image's capture
//! metadata: the white-balance matrix and the baseline exposure to apply.

use crate::capture::CaptureInfo;
use crate::color::{absolute_white_balance_matrix, to_f32};
use crate::graph::{ImageId, Node};

/// Where the baseline exposure of a Develop node came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaselineSource {
    /// The baseline toggle is off.
    Off,
    /// The value stored in the file (DNG BaselineExposure).
    File,
    /// The node's `baseline_default` parameter (RAW without a stored value).
    Default,
    /// Rendered or unknown source: never brightened.
    NotRaw,
}

/// What a Develop node does to one image.
#[derive(Clone, Debug, PartialEq)]
pub struct DevelopSettings {
    /// ACEScg white-balance matrix; `None` = no white-balance change.
    pub matrix: Option<[[f32; 3]; 3]>,
    /// Total exposure gain actually applied, in stops: `baseline_ev + brightness_ev`.
    pub gain_ev: f32,
    /// Baseline part (file value or default) alone; 0 when off or not RAW.
    pub baseline_ev: f32,
    /// Rendering offset (`brightness` parameter); 0 when the source is not RAW.
    pub brightness_ev: f32,
    /// (kelvin, tint) used as reference; (6500, 0) when the file has none.
    pub as_shot: (f32, f32),
    /// (kelvin, tint) in effect.
    pub effective: (f32, f32),
    pub baseline_source: BaselineSource,
}

const DEFAULT_AS_SHOT: (f32, f32) = (6500.0, 0.0);

/// Resolves a Develop node for `image`. `capture` is the image's metadata, if known.
pub fn develop_settings(node: &Node, image: ImageId, capture: Option<&CaptureInfo>) -> DevelopSettings {
    let as_shot = (
        capture.and_then(|c| c.as_shot_kelvin).filter(|k| k.is_finite() && *k > 0.0).unwrap_or(DEFAULT_AS_SHOT.0),
        capture.and_then(|c| c.as_shot_tint).filter(|t| t.is_finite()).unwrap_or(DEFAULT_AS_SHOT.1),
    );

    let (effective, matrix) = if node.choice("wb", image) == 1 {
        let target = (node.param("temperature", image), node.param("tint", image));
        let same = (target.0 - as_shot.0).abs() < 1e-3 && (target.1 - as_shot.1).abs() < 1e-3;
        let matrix = (!same).then(|| {
            to_f32(&absolute_white_balance_matrix(
                (f64::from(as_shot.0), f64::from(as_shot.1)),
                (f64::from(target.0), f64::from(target.1)),
            ))
        });
        (target, matrix)
    } else {
        (as_shot, None)
    };

    // Legacy fields remain parseable but no longer affect pixels. RAW gain is
    // applied by the decoder exactly once, even without a Develop node.
    let (baseline_ev, brightness_ev, gain_ev, baseline_source) = (0.0, 0.0, 0.0, BaselineSource::Off);

    DevelopSettings { matrix, gain_ev, baseline_ev, brightness_ev, as_shot, effective, baseline_source }
}

/// The tone part of a Develop node, applied after the base part (white balance,
/// baseline, brightness) in this order: exposure (gain 2^ev), whites/blacks,
/// highlights/shadows, contrast (pivot 0.18), vibrance, saturation (`1 + saturation`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DevelopTone {
    /// Stops, -5..=5.
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub vibrance: f32,
    pub saturation: f32,
}

impl DevelopTone {
    /// True if every value is exactly 0 (the tone part changes nothing).
    pub fn is_identity(&self) -> bool {
        *self == DevelopTone::default()
    }
}

/// Tone values of a Develop node for `image`; non-finite values read as 0.
/// Other node kinds give the identity.
pub fn develop_tone(node: &Node, image: ImageId) -> DevelopTone {
    if node.kind != crate::graph::NodeKind::Develop {
        return DevelopTone::default();
    }
    let get = |name: &str| {
        let v = node.param(name, image);
        if v.is_finite() {
            v
        } else {
            0.0
        }
    };
    DevelopTone {
        exposure: get("exposure"),
        contrast: get("contrast"),
        highlights: get("highlights"),
        shadows: get("shadows"),
        whites: get("whites"),
        blacks: get("blacks"),
        vibrance: get("vibrance"),
        saturation: get("saturation"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Graph, NodeKind};

    const A: ImageId = ImageId(1);

    fn develop_node(edit: impl FnOnce(&mut Node)) -> Node {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::Develop, [0.0, 0.0]);
        let mut n = g.node(id).unwrap().clone();
        edit(&mut n);
        n
    }

    fn raw(kelvin: Option<f32>, tint: Option<f32>, baseline: Option<f32>) -> CaptureInfo {
        CaptureInfo {
            raw: Some(true),
            as_shot_kelvin: kelvin,
            as_shot_tint: tint,
            baseline_exposure: baseline,
            ..Default::default()
        }
    }

    fn apply(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
        let mut o = [0.0; 3];
        for r in 0..3 {
            o[r] = m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2];
        }
        o
    }

    #[test]
    fn as_shot_has_no_matrix() {
        let n = develop_node(|_| {});
        let s = develop_settings(&n, A, Some(&raw(Some(5200.0), Some(8.0), None)));
        assert_eq!(s.matrix, None);
        assert_eq!(s.as_shot, (5200.0, 8.0));
        assert_eq!(s.effective, (5200.0, 8.0));
    }

    #[test]
    fn custom_equal_to_as_shot_has_no_matrix() {
        let n = develop_node(|n| {
            n.set_param("wb", A, 1.0);
            n.set_param("temperature", A, 5200.0);
            n.set_param("tint", A, 8.0);
        });
        let s = develop_settings(&n, A, Some(&raw(Some(5200.0), Some(8.0), None)));
        assert_eq!(s.matrix, None);
        assert_eq!(s.effective, (5200.0, 8.0));
    }

    #[test]
    fn custom_different_warms_or_cools() {
        let cap = raw(Some(5200.0), Some(0.0), None);
        let warm = develop_node(|n| {
            n.set_param("wb", A, 1.0);
            n.set_param("temperature", A, 8000.0);
        });
        let m = develop_settings(&warm, A, Some(&cap)).matrix.expect("matrix");
        let w = apply(&m, [1.0, 1.0, 1.0]);
        assert!(w[0] > w[2], "{w:?}");
        let cool = develop_node(|n| {
            n.set_param("wb", A, 1.0);
            n.set_param("temperature", A, 3200.0);
        });
        let m = develop_settings(&cool, A, Some(&cap)).matrix.expect("matrix");
        let w = apply(&m, [1.0, 1.0, 1.0]);
        assert!(w[2] > w[0], "{w:?}");
        let magenta = develop_node(|n| {
            n.set_param("wb", A, 1.0);
            n.set_param("temperature", A, 5200.0);
            n.set_param("tint", A, 40.0);
        });
        let m = develop_settings(&magenta, A, Some(&cap)).matrix.expect("matrix");
        let w = apply(&m, [1.0, 1.0, 1.0]);
        assert!(w[1] < w[0] && w[1] < w[2], "{w:?}");
    }

    #[test]
    fn missing_as_shot_values_fall_back_to_6500_0() {
        let n = develop_node(|n| {
            n.set_param("wb", A, 1.0);
            n.set_param("temperature", A, 6500.0);
        });
        for cap in [None, Some(raw(None, None, None))] {
            let s = develop_settings(&n, A, cap.as_ref());
            assert_eq!(s.as_shot, (6500.0, 0.0));
            assert_eq!(s.matrix, None);
        }
        let s = develop_settings(&n, A, Some(&raw(Some(4000.0), None, None)));
        assert_eq!(s.as_shot, (4000.0, 0.0));
        assert!(s.matrix.is_some());
    }

    fn parts(s: &DevelopSettings) -> (BaselineSource, f32, f32, f32) {
        (s.baseline_source, s.baseline_ev, s.brightness_ev, s.gain_ev)
    }

    #[test]
    fn legacy_baseline_values_do_not_add_node_gain() {
        let n = develop_node(|n| { n.set_param("baseline", A, 0.0); n.set_param("baseline_default", A, 1.0); n.set_param("brightness", A, 2.0); });
        for cap in [Some(raw(Some(5200.0), None, Some(0.5))), Some(CaptureInfo { raw: Some(false), ..Default::default() }), None] {
            assert_eq!(develop_settings(&n, A, cap.as_ref()).gain_ev, 0.0);
        }
    }

    #[test]
    fn baseline_and_brightness_parts() {
        let n = develop_node(|_| {});
        let off = develop_node(|n| { n.set_param("baseline", A, 0.0); n.set_param("baseline_default", A, -0.5); n.set_param("brightness", A, -1.0); });
        let tagged = raw(None, None, Some(0.5));
        let untagged = raw(None, None, None);
        let rendered = CaptureInfo { raw: Some(false), baseline_exposure: Some(0.5), ..Default::default() };
        for node in [&n, &off] {
            for cap in [Some(&tagged), Some(&untagged), Some(&rendered), None] {
                assert_eq!(parts(&develop_settings(node, A, cap)), (BaselineSource::Off, 0.0, 0.0, 0.0));
            }
        }
    }
}
