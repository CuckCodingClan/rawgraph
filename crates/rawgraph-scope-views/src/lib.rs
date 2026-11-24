//! egui widgets for the RawGraph scopes: histogram, waveform, RGB parade and
//! vectorscope, drawn from the data computed by `rawgraph-scopes`, with
//! DaVinci-Resolve-style graticules, hover readouts and an adaptive layout.
//!
//! * [`ScopePanel`] bundles all four views with a toolbar. Feed it with
//!   [`ScopePanel::set_image`] or, off the UI thread, with
//!   [`ScopePanel::prepare_settings`] -> [`prepare`] -> [`ScopePanel::set_prepared`].
//! * [`WaveformView`], [`ParadeView`], [`VectorscopeView`], [`HistogramView`] are the
//!   individual stateful widgets.
//! * Colorize (`ScopeStyle::colourise` / `PrepareSettings::colourise`, default on) colours the
//!   scopes like the picture: luma waveform per bin, RGB overlay in pure red / green / blue,
//!   vectorscope by position, histogram areas that add up to white. Off is the previous look.
//! * [`render`], [`geometry`], [`readout`] and [`layout`] are pure, unit-tested helpers.

pub mod geometry;
pub mod layout;
mod panel;
pub mod prepare;
pub mod readout;
pub mod render;
mod style;
#[cfg(test)]
mod tests_colour;
mod views;

pub use layout::{PanelLayout, ScopeKind};
pub use panel::ScopePanel;
pub use prepare::{
    prepare, PrepareSettings, PreparedHistogram, PreparedScopes, PreparedTrace, HISTOGRAM_BINS,
    PARADE_COLUMNS, PARADE_LEVELS, VECTORSCOPE_SIZE, WAVEFORM_COLUMNS, WAVEFORM_LEVELS,
};
pub use rawgraph_scopes::{ClipStats, ParadeMode};
pub use style::{
    parade_mode_label, plane_tints, HistogramMode, ScaleKind, ScopeStyle, WaveformMode,
    CB_TINT, CHANNEL_TINTS, CR_TINT, LUMA_TINT, PRIMARY_TINTS,
};
pub use views::{HistogramView, ParadeView, VectorscopeView, WaveformView};
