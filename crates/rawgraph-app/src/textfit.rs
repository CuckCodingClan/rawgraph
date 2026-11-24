//! Shortening text to a width with an ellipsis: the head of a file name in
//! the filmstrip, the tail of a long folder path in a text field.

use eframe::egui::{self, Color32, FontId};

const ELLIPSIS: char = '\u{2026}';

/// `text` unchanged when it fits `max_w`, else its start followed by an ellipsis
/// that fits. `measure` gives the width of a string.
pub fn fit_start(text: &str, max_w: f32, measure: &dyn Fn(&str) -> f32) -> String {
    if measure(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let build = |n: usize| -> String {
        let mut s: String = chars[..n].iter().collect();
        s.push(ELLIPSIS);
        s
    };
    // Largest n with a fitting result (the width grows with n).
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&build(mid)) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    build(lo)
}

/// `text` unchanged when it fits `max_w`, else an ellipsis followed by its end.
pub fn fit_end(text: &str, max_w: f32, measure: &dyn Fn(&str) -> f32) -> String {
    if measure(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let build = |n: usize| -> String {
        let mut s = String::from(ELLIPSIS);
        s.extend(chars[chars.len() - n..].iter());
        s
    };
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&build(mid)) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    build(lo)
}

/// Width of `text` in `font` as the painter lays it out.
pub fn text_width(painter: &egui::Painter, text: &str, font: &FontId) -> f32 {
    painter.layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE).size().x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(s: &str) -> f32 {
        s.chars().count() as f32 * 7.0
    }

    #[test]
    fn short_text_is_unchanged() {
        assert_eq!(fit_start("abc", 100.0, &mono), "abc");
        assert_eq!(fit_end("abc", 100.0, &mono), "abc");
    }

    #[test]
    fn the_head_or_the_tail_stays_and_the_result_fits() {
        let text = "20260926_Smith_Jones-346.jpg.png";
        for w in [0.0, 6.0, 7.0, 30.0, 70.0, 150.0, 200.0] {
            let a = fit_start(text, w, &mono);
            let b = fit_end(text, w, &mono);
            if w >= 7.0 {
                assert!(mono(&a) <= w && mono(&b) <= w, "width {w}: {a} / {b}");
            }
            assert!(a.ends_with(ELLIPSIS) && b.starts_with(ELLIPSIS));
        }
        let b = fit_end("D:/Photos/2026/Smith/exports/final", 70.0, &mono);
        assert!(b.ends_with("final") && b.starts_with(ELLIPSIS), "{b}");
        let a = fit_start(text, 70.0, &mono);
        assert!(a.starts_with("2026"), "{a}");
    }

    #[test]
    fn multi_byte_text_is_cut_on_characters() {
        let a = fit_start("\u{e4}\u{f6}\u{fc}\u{e4}\u{f6}\u{fc}\u{e4}\u{f6}\u{fc}", 30.0, &mono);
        assert!(a.chars().count() <= 4 + 1);
        let b = fit_end("\u{e4}\u{f6}\u{fc}\u{e4}\u{f6}\u{fc}", 30.0, &mono);
        assert!(b.starts_with(ELLIPSIS));
    }
}
