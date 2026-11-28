//! The filmstrip header state and its mapping to `rawgraph_project::Filter`.

use rawgraph_project::{Filter, Flag, KeywordMatch, SortKey};

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
    /// Keywords the images must carry (see `keyword_match`); empty = no keyword filter.
    pub keywords: Vec<String>,
    /// Any or all of `keywords`.
    pub keyword_match: KeywordMatch,
    /// Only images without keywords.
    pub no_keywords: bool,
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
            keywords: Vec::new(),
            keyword_match: KeywordMatch::Any,
            no_keywords: false,
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
            keywords: self.keywords.clone(),
            keyword_match: self.keyword_match,
            no_keywords: self.no_keywords,
            // Missing files stay visible (with a marker) so they can be relinked.
            include_missing: true,
        }
    }

    pub fn is_active(&self) -> bool {
        let d = Self::default();
        self.text != d.text || self.min_rating != 0 || self.flag.is_some() || !self.iso_min.is_empty() || !self.iso_max.is_empty() || self.flash.is_some() || self.missing_only || self.keyword_filter_active()
    }

    /// A keyword filter is set (selected keywords or "no keywords").
    pub fn keyword_filter_active(&self) -> bool {
        self.no_keywords || !self.keywords.is_empty()
    }

    /// Adds or removes a keyword of the filter (case-insensitive); returns whether it is selected now.
    pub fn toggle_keyword(&mut self, keyword: &str) -> bool {
        let key = rawgraph_core::keywords::keyword_key(keyword);
        let before = self.keywords.len();
        self.keywords.retain(|k| rawgraph_core::keywords::keyword_key(k) != key);
        if self.keywords.len() == before {
            self.keywords.push(keyword.to_owned());
            self.no_keywords = false;
            return true;
        }
        false
    }

    /// The text of the filter button.
    pub fn keyword_summary(&self) -> String {
        if self.no_keywords {
            "No keywords".to_owned()
        } else {
            match self.keywords.len() {
                0 => "Keywords".to_owned(),
                1 => rawgraph_core::keywords::keyword_leaf(&self.keywords[0]).to_owned(),
                n => format!("{n} keywords ({})", if self.keyword_match == KeywordMatch::All { "all" } else { "any" }),
            }
        }
    }
}

pub fn sort_label(k: SortKey) -> &'static str {
    match k {
        SortKey::FileName => "Name",
        SortKey::ModifiedDate => "Modified date",
        SortKey::CaptureDate => "Capture date",
        SortKey::Iso => "ISO",
        SortKey::Ev100 => "EV100",
        SortKey::Rating => "Rating",
    }
}

pub const SORT_KEYS: [SortKey; 6] = [SortKey::FileName, SortKey::ModifiedDate, SortKey::CaptureDate, SortKey::Iso, SortKey::Ev100, SortKey::Rating];

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
            ..Default::default()
        };
        let f = ui.to_filter();
        assert_eq!(f.text.as_deref(), Some("_M4"));
        assert_eq!((f.min_rating, f.flag, f.flash_fired), (Some(3), Some(Flag::Pick), Some(true)));
        assert_eq!((f.iso_min, f.iso_max), (Some(100.0), None), "garbage means no bound");
        assert!(ui.is_active());
    }

    #[test]
    fn the_keyword_filter_maps_and_toggles() {
        let mut ui = FilterUi::default();
        assert!(!ui.keyword_filter_active());
        assert!(ui.toggle_keyword("Reise|Graz"));
        assert!(ui.toggle_keyword("Food"));
        assert!(!ui.toggle_keyword("reise|graz"), "case-insensitive: the same keyword again removes it");
        assert_eq!(ui.keywords, vec!["Food".to_owned()]);
        assert!(ui.is_active());
        ui.keyword_match = KeywordMatch::All;
        let f = ui.to_filter();
        assert_eq!((f.keywords, f.keyword_match, f.no_keywords), (vec!["Food".to_owned()], KeywordMatch::All, false));
        assert_eq!(ui.keyword_summary(), "Food");
        ui.no_keywords = true;
        assert_eq!(ui.keyword_summary(), "No keywords");
        assert!(ui.to_filter().no_keywords);
        ui.no_keywords = false;
        assert!(ui.toggle_keyword("Reise|Graz"));
        assert_eq!(ui.keyword_summary(), "2 keywords (all)");
        assert!(ui.toggle_keyword("Other"), "adding a keyword leaves \"no keywords\" off");
    }
}
