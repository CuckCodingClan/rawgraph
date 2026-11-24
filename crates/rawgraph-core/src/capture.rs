//! Capture metadata and the capture-derived exposure value.

use serde::{Deserialize, Serialize};

/// Exposure-relevant metadata of one capture. Every field may be missing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureInfo {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    /// ISO speed (arithmetic).
    pub iso: Option<f32>,
    /// Shutter time in seconds.
    pub shutter_s: Option<f32>,
    /// Aperture as f-number.
    pub aperture: Option<f32>,
    pub flash_fired: Option<bool>,
    /// `Some(true)` = RAW/DNG sensor data, `Some(false)` = rendered file
    /// (JPEG/PNG/TIFF), `None` = unknown.
    #[serde(default)]
    pub raw: Option<bool>,
    /// White balance the file was developed with.
    #[serde(default)]
    pub as_shot_kelvin: Option<f32>,
    #[serde(default)]
    pub as_shot_tint: Option<f32>,
    /// DNG BaselineExposure in stops, when the file carries one.
    #[serde(default)]
    pub baseline_exposure: Option<f32>,
    /// Absolute path of the source file. Filled in by the image provider at
    /// lookup time (it is the project's business, not the capture's), so it is
    /// never written to files.
    #[serde(skip)]
    pub path: Option<String>,
}

/// Normalised form used for path comparisons: backslash read as `/`, repeated
/// separators collapsed, lower case.
fn normalize_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_sep = false;
    for c in s.chars() {
        if c == '/' || c == '\\' {
            if !last_sep {
                out.push('/');
            }
            last_sep = true;
        } else {
            out.extend(c.to_lowercase());
            last_sep = false;
        }
    }
    out
}

/// True if `path` lies inside `folder`: directly, or at any depth when
/// `recursive`. `/` and backslash are the same separator, case is ignored, trailing
/// separators of the folder are ignored. The folder itself is not inside
/// itself, and an empty `folder` contains nothing (callers treat "no folder"
/// as "everything" before asking).
pub fn path_in_folder(path: &str, folder: &str, recursive: bool) -> bool {
    if folder.is_empty() {
        return false;
    }
    let folder = normalize_path(folder);
    let path = normalize_path(path);
    let mut prefix = folder.trim_end_matches('/').to_owned();
    prefix.push('/');
    match path.strip_prefix(prefix.as_str()) {
        Some(rest) if !rest.is_empty() => recursive || !rest.contains('/'),
        _ => false,
    }
}

/// Last path component, lower-cased: the key under which a Merge compares file names.
pub(crate) fn name_key(path: &str) -> Option<String> {
    let last = path.rsplit(['/', '\\']).next().unwrap_or("");
    (!last.is_empty()).then(|| last.to_lowercase())
}

/// A numeric capture measure that a metadata filter can test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    Iso,
    ShutterSeconds,
    Aperture,
    Ev100,
    Flash,
}

impl Measure {
    /// Display labels, indexed like [`Measure::from_index`].
    pub const LABELS: [&'static str; 5] = ["ISO", "Shutter time (s)", "Aperture (f)", "EV100", "Flash"];

    pub fn from_index(i: usize) -> Option<Measure> {
        match i {
            0 => Some(Measure::Iso),
            1 => Some(Measure::ShutterSeconds),
            2 => Some(Measure::Aperture),
            3 => Some(Measure::Ev100),
            4 => Some(Measure::Flash),
            _ => None,
        }
    }
}

impl CaptureInfo {
    /// Value of one measure; `None` when the capture does not record it.
    /// Flash is 1.0 when fired, 0.0 when not fired.
    pub fn measure(&self, m: Measure) -> Option<f32> {
        match m {
            Measure::Iso => self.iso,
            Measure::ShutterSeconds => self.shutter_s,
            Measure::Aperture => self.aperture,
            Measure::Ev100 => self.ev100(),
            Measure::Flash => self.flash_fired.map(|f| if f { 1.0 } else { 0.0 }),
        }
    }

    /// Capture-derived exposure value, referenced to ISO 100:
    ///
    /// `EV100 = log2(N² / t) − log2(ISO / 100)`
    ///
    /// Higher means a brighter scene. Returns `None` when ISO, shutter time or
    /// aperture is missing or not positive (for example adapted manual lenses
    /// that report f/0), so callers must treat the value as unknown instead of
    /// inventing one.
    pub fn ev100(&self) -> Option<f32> {
        let (iso, t, n) = (self.iso?, self.shutter_s?, self.aperture?);
        if iso > 0.0 && t > 0.0 && n > 0.0 {
            Some((n * n / t).log2() - (iso / 100.0).log2())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(iso: f32, t: f32, n: f32) -> CaptureInfo {
        CaptureInfo { iso: Some(iso), shutter_s: Some(t), aperture: Some(n), ..Default::default() }
    }

    #[test]
    fn new_fields_round_trip_and_old_json_still_loads() {
        let c = CaptureInfo {
            iso: Some(100.0),
            raw: Some(true),
            as_shot_kelvin: Some(5200.0),
            as_shot_tint: Some(-4.5),
            baseline_exposure: Some(0.75),
            ..Default::default()
        };
        let back: CaptureInfo = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        let old = r#"{"make":"X","model":null,"lens":null,"iso":200.0,"shutter_s":null,"aperture":null,"flash_fired":null}"#;
        let loaded: CaptureInfo = serde_json::from_str(old).unwrap();
        assert_eq!(loaded.make.as_deref(), Some("X"));
        assert_eq!(
            (loaded.raw, loaded.as_shot_kelvin, loaded.as_shot_tint, loaded.baseline_exposure),
            (None, None, None, None)
        );
        let minimal: CaptureInfo = serde_json::from_str("{}").unwrap();
        assert_eq!(minimal, CaptureInfo::default());
    }

    #[test]
    fn measures_expose_the_recorded_values() {
        let mut c = info(400.0, 0.01, 2.8);
        assert_eq!(c.measure(Measure::Iso), Some(400.0));
        assert_eq!(c.measure(Measure::ShutterSeconds), Some(0.01));
        assert_eq!(c.measure(Measure::Aperture), Some(2.8));
        assert_eq!(c.measure(Measure::Ev100), c.ev100());
        assert_eq!(c.measure(Measure::Flash), None);
        c.flash_fired = Some(true);
        assert_eq!(c.measure(Measure::Flash), Some(1.0));
        c.flash_fired = Some(false);
        assert_eq!(c.measure(Measure::Flash), Some(0.0));
        assert_eq!(CaptureInfo::default().measure(Measure::Ev100), None);
    }

    #[test]
    fn measure_indices_match_the_labels() {
        for i in 0..Measure::LABELS.len() {
            assert!(Measure::from_index(i).is_some());
        }
        assert_eq!(Measure::from_index(0), Some(Measure::Iso));
        assert_eq!(Measure::from_index(4), Some(Measure::Flash));
        assert_eq!(Measure::from_index(5), None);
    }

    #[test]
    fn ev100_reference_points() {
        assert!((info(100.0, 1.0, 1.0).ev100().unwrap()).abs() < 1e-6);
        // "Sunny 16": f/16, 1/125 s, ISO 100 is about EV 15.
        assert!((info(100.0, 1.0 / 125.0, 16.0).ev100().unwrap() - 14.966).abs() < 1e-2);
        // Doubling ISO at the same settings means the scene was one stop darker.
        let a = info(100.0, 1.0 / 60.0, 4.0).ev100().unwrap();
        let b = info(200.0, 1.0 / 60.0, 4.0).ev100().unwrap();
        assert!((a - b - 1.0).abs() < 1e-5);
    }

    #[test]
    fn ev100_is_unknown_for_missing_or_invalid_metadata() {
        assert_eq!(CaptureInfo::default().ev100(), None);
        assert_eq!(info(100.0, 1.0, 0.0).ev100(), None);
        assert_eq!(info(0.0, 1.0, 2.8).ev100(), None);
    }
}
