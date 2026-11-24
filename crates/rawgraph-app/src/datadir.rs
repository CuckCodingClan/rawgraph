//! Where the application keeps its own data: recovery copies, the thumbnail
//! cache and recipes. Everything lives under one root (by default
//! `%LOCALAPPDATA%\rawgraph`) so tests and `--data-dir` can redirect all of it
//! with one value. The application never writes anywhere else except to files
//! the user saves or exports.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataDirs {
    root: PathBuf,
}

impl DataDirs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The per-user default: `%LOCALAPPDATA%\rawgraph`, or a folder in the
    /// system temp directory when that variable is missing.
    pub fn standard() -> Self {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self::new(base.join("rawgraph"))
    }

    /// `--data-dir` if given, else the standard location.
    pub fn from_option(dir: Option<&Path>) -> Self {
        dir.map_or_else(Self::standard, Self::new)
    }

    #[cfg(test)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn recovery(&self) -> PathBuf {
        self.root.join("recovery")
    }

    pub fn thumbs(&self) -> PathBuf {
        self.root.join("cache").join("thumbs")
    }

    pub fn recipes(&self) -> PathBuf {
        self.root.join("recipes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_hangs_below_one_root() {
        let d = DataDirs::new("X:/data");
        assert_eq!(d.recovery(), PathBuf::from("X:/data/recovery"));
        assert_eq!(d.thumbs(), PathBuf::from("X:/data/cache/thumbs"));
        assert_eq!(d.recipes(), PathBuf::from("X:/data/recipes"));
        assert_eq!(DataDirs::from_option(Some(Path::new("Y:/z"))).root(), Path::new("Y:/z"));
        assert!(DataDirs::standard().root().ends_with("rawgraph"));
    }
}
