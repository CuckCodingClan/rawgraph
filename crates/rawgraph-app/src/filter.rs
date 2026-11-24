//! The filmstrip header state and its mapping to `rawgraph_project::Filter`.

use rawgraph_project::{Filter, Flag, SortKey};

#[derive(Clone, Debug, PartialEq)]
pub struct FilterUi {
    pub text: String,
    /// 0 = any.
    pub min_rating: u8,
    pub flag: Option<Flag>,
    /// Empty or unparsable text means "no bound".
    pub iso_min: String,
    pub iso_max: String,
    pub flash: Option<bool>,
    pub sort: SortKey,
    pub descending: bool,
    /// View > Show missing files only.
    pub missing_only: bool,
}

impl Default for FilterUi {
    fn default() -> Self {
        Self {
            text: String::new(),
            min_rating: 0,
            flag: None,
            iso_min: String::new(),
            iso_max: String::new(),
            flash: None,
            sort: SortKey::FileName,
            descending: false,
            missing_only: false,
        }
    }
}

fn parse_bound(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0)
}

impl FilterUi {
    pub fn to_filter(&self) -> Filter {
        let text = self.text.trim();
        Filter {
            iso_min: parse_bound(&self.iso_min),
            iso_max: parse_bound(&self.iso_max),
            min_rating: (self.min_rating > 0).then_some(self.min_rating),
            flag: self.flag,
            flash_fired: self.flash,
            text: (!text.is_empty()).then(|| text.to_owned()),
            // Missing files stay visible (with a marker) so they can be relinked.
            include_missing: true,
        }
    }

    pub fn is_active(&self) -> bool {
        let d = Self::default();
        self.text != d.text || self.min_rating != 0 || self.flag.is_some() || !self.iso_min.is_empty() || !self.iso_max.is_empty() || self.flash.is_some() || self.missing_only
    }
}

pub fn sort_label(k: SortKey) -> &'static str {
    match k {
        SortKey::FileName => "Name",
        SortKey::Iso => "ISO",
        SortKey::Ev100 => "EV100",
        SortKey::Rating => "Rating",
    }
}

pub const SORT_KEYS: [SortKey; 4] = [SortKey::FileName, SortKey::Iso, SortKey::Ev100, SortKey::Rating];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_header_means_no_filter() {
        let f = FilterUi::default().to_filter();
        assert_eq!((f.iso_min, f.iso_max, f.min_rating, f.flag, f.flash_fired, f.text), (None, None, None, None, None, None));
        assert!(!FilterUi::default().is_active());
    }

    #[test]
    fn header_maps_to_filter_fields() {
        let ui = FilterUi {
            text: "  _M4 ".into(),
            min_rating: 3,
            flag: Some(Flag::Pick),
            iso_min: "100".into(),
            iso_max: "abc".into(),
            flash: Some(true),
            sort: SortKey::Iso,
            descending: true,
            missing_only: false,
        };
        let f = ui.to_filter();
        assert_eq!(f.text.as_deref(), Some("_M4"));
        assert_eq!((f.min_rating, f.flag, f.flash_fired), (Some(3), Some(Flag::Pick), Some(true)));
        assert_eq!((f.iso_min, f.iso_max), (Some(100.0), None), "garbage means no bound");
        assert!(ui.is_active());
    }
}
