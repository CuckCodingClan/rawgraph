//! Locations of the private sample photos and of generated pictures (never committed).
//!
//! `RAWGRAPH_SAMPLES` overrides the sample folder, `RAWGRAPH_OUT` the output folder. Defaults:
//! `%LOCALAPPDATA%\rawgraph\samples` and `%LOCALAPPDATA%\rawgraph\shots`
//! (elsewhere `$HOME/.local/share/rawgraph/...`).
#![allow(dead_code)]

use std::path::PathBuf;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn base_dir() -> PathBuf {
    if let Some(d) = env_path("LOCALAPPDATA") {
        d.join("rawgraph")
    } else if let Some(h) = env_path("HOME") {
        h.join(".local").join("share").join("rawgraph")
    } else {
        PathBuf::from("rawgraph")
    }
}

/// Folder with the sample photos.
pub fn samples_dir() -> PathBuf {
    env_path("RAWGRAPH_SAMPLES").unwrap_or_else(|| base_dir().join("samples"))
}

/// Folder for generated pictures.
pub fn out_dir() -> PathBuf {
    env_path("RAWGRAPH_OUT").unwrap_or_else(|| base_dir().join("shots"))
}
