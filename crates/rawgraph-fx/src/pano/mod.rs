//! Panorama stitching maths (CPU, pure Rust): feature matching, homography RANSAC, global
//! alignment, cylindrical / spherical / planar projection, exposure compensation, multiband
//! blending and crop. Standalone; no graph integration.
//!
//! Pipeline of [`stitch`]:
//! 1. log-luminance work images (`work_px` long side), Harris corners over 3 octaves with oriented
//!    256-bit BRIEF descriptors;
//! 2. all-pairs brute-force Hamming matching (ratio test 0.8 + cross check);
//! 3. normalised-DLT RANSAC homographies on sub-pixel, full-resolution coordinates (3 work px,
//!    2000 iterations at most), refined by a robust Levenberg-Marquardt fit; a pair is connected
//!    with at least 30 inliers, an inlier ratio of at least 0.3 and a plausible homography;
//! 4. maximum spanning tree on the inlier counts, reference = the most central frame; focal length
//!    from the pair homographies (least-squares rotation-consistency form of the Szeliski estimate)
//!    unless given; rotations from `K^-1 H K`, then a global Levenberg-Marquardt over all rotations
//!    and the shared focal length on the reprojection error of all connected pairs, and horizon
//!    straightening (wave correction) for the cylindrical / spherical projections; the chained
//!    homographies directly for the planar one;
//! 5. exposure gains from the median overlap ratios (least squares, reference gain 1);
//! 6. multiband blending (Laplacian pyramid, seams from per-frame edge-distance weights) in
//!    512 px tiles with an aligned margin, or plain distance feathering;
//! 7. optional crop to the largest axis-aligned rectangle fully inside the coverage.
//!
//! Frames are linear RGBA `ImageBuf`s; the output is float with alpha = coverage (0 / 1).

// Index loops over small matrices and pixel arrays read clearer than iterator chains here.
#![allow(clippy::needless_range_loop)]

mod align;
mod blend;
mod bundle;
mod compose;
mod features;
mod matching;
mod ransac;
mod warp;

#[cfg(test)]
mod tests;

use rawgraph_core::ImageBuf;
use rayon::prelude::*;

use self::align::{Geo, Pair};
use self::bundle::{refine_rotations, wave_correction, BundleInput};
use self::blend::{blend_feather, blend_multiband, TileInput};
use self::compose::{exposure_gains, largest_rect, ExpFrame};
use self::ransac::{inv, mul, normalized, IDENT};
use self::warp::{edge_weight, render_frame_tile, Warper};

/// Output projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projection {
    Cylindrical,
    Spherical,
    Planar,
}

/// Blend mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Blend {
    /// Laplacian pyramid blend with this many levels (clamped to 1..=6).
    Multiband { levels: u32 },
    /// Plain edge-distance weighted average.
    Feather,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanoParams {
    pub projection: Projection,
    /// Feature resolution (long side of the work images).
    pub work_px: u32,
    pub blend: Blend,
    pub exposure_comp: bool,
    /// Crop to the largest valid rectangle.
    pub crop: bool,
    /// Focal length in pixels at the full resolution of the first frame (e.g. from EXIF);
    /// estimated from the frames when `None`.
    pub focal_px: Option<f32>,
    /// Keep `focal_px` exactly (no refinement). Otherwise a given focal length is the start value and
    /// the global refinement may move it by up to +-10 %.
    pub focal_fixed: bool,
    /// Exposure estimate: samples with a channel at or above this level are treated as clipped
    /// (e.g. `Some(0.98)` for display-referred data). `None` for scene-linear data; the brightest
    /// 1 % of every frame is always excluded.
    pub exposure_clip: Option<f32>,
    /// Memory the output canvas may take (bytes); [`MAX_BYTES`] when `None`. The caller knows how
    /// much the machine has and what its frames already hold.
    pub max_output_bytes: Option<u64>,
}

impl Default for PanoParams {
    fn default() -> Self {
        Self {
            projection: Projection::Cylindrical,
            work_px: 1200,
            blend: Blend::Multiband { levels: 5 },
            exposure_comp: true,
            crop: true,
            focal_px: None,
            focal_fixed: false,
            exposure_clip: None,
            max_output_bytes: None,
        }
    }
}

/// Pixel rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Debug)]
pub struct Panorama {
    /// RGBA float; alpha is the coverage (1 inside, 0 outside). Already cropped when
    /// `PanoParams::crop` was set.
    pub image: ImageBuf,
    /// Largest axis-aligned rectangle fully inside the coverage, in the coordinates of `image`
    /// (the whole image when it was cropped; empty when there is none).
    pub valid_crop: Rect,
    /// Frame indices in panorama order (left to right).
    pub order: Vec<usize>,
    /// Focal length used, in pixels at the full resolution of the first frame.
    pub focal_px: f32,
    /// Position of the image's top-left corner in the projection plane: pixels relative to the
    /// projection centre, which is the centre of the reference frame (the reference frame's pixel
    /// `(x, y)` is at `(x + 0.5 - w/2, y + 0.5 - h/2)` for the planar projection; for the cylindrical
    /// and spherical ones the reference viewing direction made horizontal, after horizon
    /// straightening).
    pub origin: [i32; 2],
    /// Index of the reference frame (the projection is centred on its viewing direction).
    pub reference: usize,
    /// Non-fatal findings (e.g. the focal length search hit its range limit).
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PanoError {
    TooFewFrames,
    TooManyFrames { count: usize },
    /// A frame is smaller than 32 px.
    InvalidFrame { frame: usize },
    /// Frames that could not be connected to the main group.
    NotConnected { frames: Vec<usize> },
    /// The output would exceed the size limit.
    TooLarge { width: u64, height: u64 },
    /// The geometry is unusable (e.g. a homography maps to infinity).
    Degenerate(String),
    Cancelled,
}

impl std::fmt::Display for PanoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PanoError::TooFewFrames => write!(f, "a panorama needs at least two frames"),
            PanoError::TooManyFrames { count } => write!(f, "{count} frames exceed the limit of {MAX_FRAMES}"),
            PanoError::InvalidFrame { frame } => write!(f, "frame {frame} is too small"),
            PanoError::NotConnected { frames } => write!(f, "frames {frames:?} do not overlap the others enough"),
            PanoError::TooLarge { width, height } => write!(
                f,
                "a panorama of {width}x{height} px needs about {:.1} GB, more than the memory available for it; stitch fewer frames or use a smaller long edge",
                (*width as f64) * (*height as f64) * 17.0 / 1e9
            ),
            PanoError::Degenerate(m) => write!(f, "degenerate geometry: {m}"),
            PanoError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for PanoError {}

/// Maximum number of frames (all-pairs matching).
pub const MAX_FRAMES: usize = 25;
/// How far (relative) the refinement may move a given, not fixed focal length.
const GIVEN_FOCAL_TOL: f64 = 0.10;
/// Output size limits.
pub const MAX_LONG_SIDE: u64 = 30_000;
pub const MAX_PIXELS: u64 = 400_000_000;
/// Memory budget of the output: float RGBA (16 bytes per pixel) plus the coverage mask (1 byte).
pub const MAX_BYTES: u64 = 4 << 30;
const MAX_FEATURES: usize = 1500;
const TILE: i64 = 512;

fn check(cancel: &(dyn Fn() -> bool + Sync)) -> Result<(), PanoError> {
    if cancel() {
        Err(PanoError::Cancelled)
    } else {
        Ok(())
    }
}

/// Rejects canvases beyond the size limits.
#[cfg(test)]
fn check_size(w: f64, h: f64) -> Result<(), PanoError> {
    check_size_within(w, h, MAX_BYTES)
}

fn check_size_within(w: f64, h: f64, max_bytes: u64) -> Result<(), PanoError> {
    if !(w.is_finite() && h.is_finite())
        || w > 1e9
        || h > 1e9
        || w * h > MAX_PIXELS as f64
        || w.max(h) > MAX_LONG_SIDE as f64
        || w * h * 17.0 > max_bytes as f64
    {
        return Err(PanoError::TooLarge { width: w.min(1e18) as u64, height: h.min(1e18) as u64 });
    }
    Ok(())
}

/// `vec![v; n]` that reports an allocation failure as [`PanoError::TooLarge`] instead of aborting.
fn try_vec<T: Clone>(n: usize, v: T, width: u64, height: u64) -> Result<Vec<T>, PanoError> {
    let mut out = Vec::new();
    out.try_reserve_exact(n).map_err(|_| PanoError::TooLarge { width, height })?;
    out.resize(n, v);
    Ok(out)
}

/// Rendered tile: output x, y, width, height and RGBA data.
type TileOut = (usize, usize, usize, usize, Vec<f32>);

/// Makes the work image finite: a NaN / Inf sample takes the value of the nearest finite sample to
/// its left in the row (0 when there is none), so a dead pixel does not become a deep black corner
/// that dominates the feature detection. (The full-resolution render treats such pixels as
/// weight 0.)
fn sanitize(img: ImageBuf) -> ImageBuf {
    if img.data().iter().all(|v| v.is_finite()) {
        return img;
    }
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut data = img.data().to_vec();
    for y in 0..h {
        let mut last = [0.0f32; 4];
        for x in 0..w {
            for c in 0..4 {
                let v = &mut data[(y * w + x) * 4 + c];
                if v.is_finite() {
                    last[c] = *v;
                } else {
                    *v = last[c];
                }
            }
        }
    }
    ImageBuf::from_rgba(img.width(), img.height(), data).unwrap_or(img)
}

struct Work {
    small: ImageBuf,
    geo: Geo,
    kps: Vec<features::Keypoint>,
}

/// Perimeter and interior sample points of a frame in centred pixel coordinates.
fn frame_samples(w: f64, h: f64) -> Vec<[f64; 2]> {
    let mut v = Vec::new();
    for t in 0..=32 {
        let u = t as f64 / 32.0;
        v.push([-w / 2.0 + u * w, -h / 2.0]);
        v.push([-w / 2.0 + u * w, h / 2.0]);
        v.push([-w / 2.0, -h / 2.0 + u * h]);
        v.push([w / 2.0, -h / 2.0 + u * h]);
    }
    for a in 1..5 {
        for b in 1..5 {
            v.push([-w / 2.0 + w * a as f64 / 5.0, -h / 2.0 + h * b as f64 / 5.0]);
        }
    }
    v
}

/// Stitches `frames` into one panorama. `cancel` is polled regularly; a `true` ends the work with
/// [`PanoError::Cancelled`].
pub fn stitch(frames: &[&ImageBuf], p: &PanoParams, cancel: &(dyn Fn() -> bool + Sync)) -> Result<Panorama, PanoError> {
    let n = frames.len();
    if n < 2 {
        return Err(PanoError::TooFewFrames);
    }
    if n > MAX_FRAMES {
        return Err(PanoError::TooManyFrames { count: n });
    }
    for (i, f) in frames.iter().enumerate() {
        if f.width() < 32 || f.height() < 32 {
            return Err(PanoError::InvalidFrame { frame: i });
        }
    }
    check(cancel)?;
    let work_px = p.work_px.clamp(256, 4096);

    // 1. Work images and features.
    let works: Vec<Work> = frames
        .par_iter()
        .map(|f| {
            let factor = f.width().max(f.height()).div_ceil(work_px).max(1);
            let small = sanitize(f.downscale_box(work_px));
            let geo = Geo {
                f: factor as f64,
                full_w: f.width() as f64,
                full_h: f.height() as f64,
            };
            let kps = if cancel() { Vec::new() } else { features::detect(&features::log_luma_plane(&small), MAX_FEATURES) };
            Work { small, geo, kps }
        })
        .collect();
    check(cancel)?;

    // 2.-3. Pairwise matching and RANSAC.
    let idx: Vec<(usize, usize)> = (0..n).flat_map(|i| ((i + 1)..n).map(move |j| (i, j))).collect();
    let pairs: Vec<Pair> = idx
        .par_iter()
        .filter_map(|&(i, j)| {
            if cancel() {
                return None;
            }
            align::align_pair(&works[i].kps, &works[i].geo, &works[j].kps, &works[j].geo, i, j)
        })
        .collect();
    check(cancel)?;

    // 4. Connectivity, reference, focal length, global transforms.
    let mut warnings: Vec<String> = Vec::new();
    let (members, rest) = align::largest_component(n, &pairs);
    if !rest.is_empty() {
        return Err(PanoError::NotConnected { frames: rest });
    }
    let tree = align::build_tree(n, &members, &pairs);
    let reference = tree.reference;
    let dims: Vec<(f64, f64)> = frames.iter().map(|f| (f.width() as f64, f.height() as f64)).collect();
    let long: Vec<f64> = dims.iter().map(|d| d.0.max(d.1)).collect();
    let given_focal = p.focal_px.filter(|f| f.is_finite() && *f > 1.0);
    let mut f_rel = match given_focal {
        Some(f) => f as f64 / long[0],
        None => match align::estimate_focal_rel(&pairs, &long) {
            Some((f, at_bound)) => {
                if at_bound {
                    warnings.push(format!(
                        "the focal length estimate hit the limit of its search range ({:.0} px); give the focal length (EXIF) for a better result",
                        f * long[0]
                    ));
                }
                f
            }
            None => {
                warnings.push("the focal length could not be estimated; a default was used".into());
                1.2
            }
        },
    };

    let mut warpers: Vec<Warper> = Vec::with_capacity(n);
    if p.projection == Projection::Planar {
        let f_out = f_rel * long[reference];
        let ms = align::chain(n, &tree, |c, _par, k| align::pair_h(&pairs[k], c))
            .ok_or_else(|| PanoError::Degenerate("homography chain".into()))?;
        for k in 0..n {
            let minv = normalized(&inv(&ms[k]).ok_or_else(|| PanoError::Degenerate("singular homography".into()))?);
            warpers.push(Warper {
                kind: p.projection,
                w: dims[k].0,
                h: dims[k].1,
                f_in: f_rel * long[k],
                f_out,
                rot: IDENT,
                m: normalized(&ms[k]),
                // Canvas -> frame: invert the frame -> canvas map (both centred).
                minv,
            });
        }
    } else {
        let mut rs = align::chain(n, &tree, |c, par, k| {
            let h = align::pair_h(&pairs[k], c)?;
            align::rotation_from_h(&h, f_rel * long[c], f_rel * long[par])
        })
        .ok_or_else(|| PanoError::Degenerate("rotation chain".into()))?;
        check(cancel)?;
        // Global refinement over ALL connected pairs.
        let input = BundleInput { pairs: &pairs, long: &long, members: &members, reference, rots: &rs, f_rel, fix_focal: given_focal.is_some() && p.focal_fixed, focal_tol: given_focal.map(|_| GIVEN_FOCAL_TOL) };
        match refine_rotations(&input, cancel) {
            Some(r) => {
                let worst = works.iter().map(|w| w.geo.f).fold(1.0, f64::max) * 3.0;
                if r.rms_after > worst {
                    warnings.push(format!(
                        "the global alignment leaves a reprojection error of {:.1} px (was {:.1} px); the frames may not share one viewpoint",
                        r.rms_after, r.rms_before
                    ));
                }
                rs = r.rots;
                f_rel = r.f_rel;
                if let Some(g) = given_focal.filter(|_| !p.focal_fixed) {
                    if (f_rel * long[0] / g as f64 - 1.0).abs() > GIVEN_FOCAL_TOL * 0.95 {
                        warnings.push(format!(
                            "the refined focal length ({:.0} px) differs from the given one ({g:.0} px) by about {:.0} %; check the focal length",
                            f_rel * long[0],
                            (f_rel * long[0] / g as f64 - 1.0).abs() * 100.0
                        ));
                    }
                }
                if given_focal.is_none() && align::at_focal_bound(f_rel) {
                    warnings.push(format!(
                        "the refined focal length reached the limit of its range ({:.0} px); give the focal length (EXIF) for a better result",
                        f_rel * long[0]
                    ));
                }
            }
            None => {
                check(cancel)?;
                warnings.push("the global alignment refinement found no improvement and was skipped".into());
            }
        }
        // Horizon straightening: re-express all rotations in the up-aligned world frame.
        if let Some(wv) = wave_correction(&rs, &members) {
            for r in rs.iter_mut() {
                *r = mul(&wv, r);
            }
        }
        let f_out = f_rel * long[reference];
        for k in 0..n {
            warpers.push(Warper {
                kind: p.projection,
                w: dims[k].0,
                h: dims[k].1,
                f_in: f_rel * long[k],
                f_out,
                rot: rs[k],
                m: IDENT,
                minv: IDENT,
            });
        }
    }
    let f_out = f_rel * long[reference];
    check(cancel)?;

    // Canvas extent from the projected frame outlines.
    let mut bboxes: Vec<[f64; 4]> = Vec::with_capacity(n);
    for k in 0..n {
        let mut bb = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for s in frame_samples(dims[k].0, dims[k].1) {
            let q = warpers[k].forward(s[0], s[1]).ok_or_else(|| {
                PanoError::Degenerate(format!("frame {k} crosses the horizon of the {:?} projection (it cannot be mapped)", p.projection))
            })?;
            bb[0] = bb[0].min(q[0]);
            bb[1] = bb[1].min(q[1]);
            bb[2] = bb[2].max(q[0]);
            bb[3] = bb[3].max(q[1]);
        }
        if bb.iter().any(|v| !v.is_finite()) {
            return Err(PanoError::Degenerate("non-finite frame outline".into()));
        }
        bboxes.push(bb);
    }
    let minx = bboxes.iter().map(|b| b[0]).fold(f64::INFINITY, f64::min);
    let miny = bboxes.iter().map(|b| b[1]).fold(f64::INFINITY, f64::min);
    let maxx = bboxes.iter().map(|b| b[2]).fold(f64::NEG_INFINITY, f64::max);
    let maxy = bboxes.iter().map(|b| b[3]).fold(f64::NEG_INFINITY, f64::max);
    if p.projection != Projection::Planar && maxx - minx > 1.95 * std::f64::consts::PI * f_out {
        return Err(PanoError::Degenerate("the frames wrap around the full circle".into()));
    }
    let (wf, hf) = ((maxx.ceil() - minx.floor()).max(0.0), (maxy.ceil() - miny.floor()).max(0.0));
    check_size_within(wf, hf, p.max_output_bytes.unwrap_or(MAX_BYTES))?;
    let (cx0, cy0) = (minx.floor() as i64, miny.floor() as i64);
    let (cw, ch) = (wf as usize, hf as usize);
    if cw == 0 || ch == 0 {
        return Err(PanoError::Degenerate("empty canvas".into()));
    }
    // Slightly padded boxes (canvas integer coordinates) for tile rejection.
    let boxes: Vec<[i64; 4]> = bboxes
        .iter()
        .map(|b| [b[0].floor() as i64 - 2, b[1].floor() as i64 - 2, b[2].ceil() as i64 + 2, b[3].ceil() as i64 + 2])
        .collect();

    // 5. Exposure compensation.
    let gains: Vec<f64> = if p.exposure_comp {
        let ef: Vec<ExpFrame> = (0..n)
            .map(|k| ExpFrame {
                small: &works[k].small,
                sx: 1.0 / works[k].geo.f,
                sy: 1.0 / works[k].geo.f,
                full_w: dims[k].0,
                full_h: dims[k].1,
                warper: &warpers[k],
                bbox: bboxes[k],
            })
            .collect();
        exposure_gains(&ef, &pairs, &members, reference, wf.max(hf), p.exposure_clip)
    } else {
        vec![1.0; n]
    };
    check(cancel)?;

    // Coverage mask and largest valid rectangle.
    let mut coverage = try_vec(cw * ch, 0u8, cw as u64, ch as u64)?;
    const ROWS: usize = 16;
    coverage.par_chunks_mut(cw * ROWS).enumerate().for_each(|(ci, chunk)| {
        if cancel() {
            return;
        }
        let rows = chunk.len() / cw;
        let y0 = ci * ROWS;
        let ys: Vec<f64> = (0..rows).map(|r| (cy0 + (y0 + r) as i64) as f64 + 0.5).collect();
        for k in 0..n {
            let b = &boxes[k];
            let (bx0, bx1) = ((b[0] - cx0).max(0) as usize, ((b[2] - cx0).max(0) as usize).min(cw));
            if bx0 >= bx1 || (cy0 + y0 as i64 + rows as i64) < b[1] || (cy0 + y0 as i64) > b[3] {
                continue;
            }
            let xs: Vec<f64> = (bx0..bx1).map(|x| (cx0 + x as i64) as f64 + 0.5).collect();
            let pos = warpers[k].map_grid(&xs, &ys);
            let (fw, fh) = (dims[k].0 as f32, dims[k].1 as f32);
            for r in 0..rows {
                for (i, x) in (bx0..bx1).enumerate() {
                    if edge_weight(pos[r * xs.len() + i], fw, fh) > 0.0 {
                        chunk[r * cw + x] = 1;
                    }
                }
            }
        }
    });
    check(cancel)?;
    let rect = largest_rect(&coverage, cw, ch, cancel)?;
    drop(coverage);
    let (region, cropped) = match (p.crop, rect) {
        (true, Some(r)) if r.w > 0 && r.h > 0 => (r, true),
        _ => (Rect { x: 0, y: 0, w: cw as u32, h: ch as u32 }, false),
    };

    // 6. Blend tile by tile.
    let levels = match p.blend {
        Blend::Multiband { levels } => levels.clamp(1, 6) as usize,
        Blend::Feather => 0,
    };
    let margin: i64 = if levels > 0 { 4 << levels } else { 0 };
    let (rw, rh) = (region.w as usize, region.h as usize);
    let mut out = try_vec(rw * rh * 4, 0.0f32, rw as u64, rh as u64)?;
    // Absolute canvas coordinates of the output region.
    let (ax0, ay0) = (cx0 + region.x as i64, cy0 + region.y as i64);
    let (ax1, ay1) = (ax0 + rw as i64, ay0 + rh as i64);
    let tx_first = ax0.div_euclid(TILE);
    let tx_last = (ax1 - 1).div_euclid(TILE);
    let ty_first = ay0.div_euclid(TILE);
    let ty_last = (ay1 - 1).div_euclid(TILE);
    for ty in ty_first..=ty_last {
        check(cancel)?;
        let tiles: Vec<Option<TileOut>> = (tx_first..=tx_last)
            .into_par_iter()
            .map(|tx| {
                if cancel() {
                    return None;
                }
                // Core (unclipped) tile and its output intersection.
                let (cx_a, cy_a) = (tx * TILE, ty * TILE);
                let (ix0, iy0) = (cx_a.max(ax0), cy_a.max(ay0));
                let (ix1, iy1) = ((cx_a + TILE).min(ax1), (cy_a + TILE).min(ay1));
                let (rx, ry) = (cx_a - margin, cy_a - margin);
                let side = (TILE + 2 * margin) as usize;
                let mut inputs: Vec<TileInput> = Vec::new();
                for k in 0..n {
                    let b = &boxes[k];
                    if b[2] <= rx || b[0] >= rx + side as i64 || b[3] <= ry || b[1] >= ry + side as i64 {
                        continue;
                    }
                    inputs.push(render_frame_tile(frames[k], &warpers[k], gains[k] as f32, rx, ry, side, side));
                }
                let (ow, oh) = ((ix1 - ix0) as usize, (iy1 - iy0) as usize);
                let mut data = vec![0.0f32; ow * oh * 4];
                if !inputs.is_empty() {
                    let (rgb, cover) = if levels > 0 {
                        blend_multiband(&inputs, side, side, levels)
                    } else {
                        blend_feather(&inputs, side, side)
                    };
                    for y in 0..oh {
                        for x in 0..ow {
                            let sp = ((iy0 - ry) as usize + y) * side + (ix0 - rx) as usize + x;
                            if cover[sp] {
                                let o = (y * ow + x) * 4;
                                data[o..o + 3].copy_from_slice(&rgb[sp * 3..sp * 3 + 3]);
                                data[o + 3] = 1.0;
                            }
                        }
                    }
                }
                Some(((ix0 - ax0) as usize, (iy0 - ay0) as usize, ow, oh, data))
            })
            .collect();
        check(cancel)?;
        for t in tiles.into_iter().flatten() {
            let (ox, oy, ow, oh, data) = t;
            for y in 0..oh {
                let dst = ((oy + y) * rw + ox) * 4;
                out[dst..dst + ow * 4].copy_from_slice(&data[y * ow * 4..(y + 1) * ow * 4]);
            }
        }
    }
    let image = ImageBuf::from_rgba(rw as u32, rh as u32, out).ok_or_else(|| PanoError::Degenerate("buffer size".into()))?;

    // Panorama order: by the projected frame centres.
    let mut order: Vec<usize> = (0..n).collect();
    let centre = |k: usize| warpers[k].forward(0.0, 0.0).map_or(f64::MAX, |q| q[0]);
    order.sort_by(|&a, &b| centre(a).total_cmp(&centre(b)).then(a.cmp(&b)));

    let valid_crop = if cropped { Rect { x: 0, y: 0, w: region.w, h: region.h } } else { rect.unwrap_or_default() };
    Ok(Panorama {
        image,
        valid_crop,
        order,
        focal_px: (f_rel * long[0]) as f32,
        origin: [ax0 as i32, ay0 as i32],
        reference,
        warnings,
    })
}
