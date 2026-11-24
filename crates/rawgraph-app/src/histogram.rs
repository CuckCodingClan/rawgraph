//! The luminance histogram behind the curve editor: the picture entering a Curve node, binned on
//! the curve's own perceptual axis `t = Y / (Y + 0.18)` (256 bins) and scaled so that one tall
//! spike does not flatten the rest. Pure functions; the render worker calls them, never the UI.

use rawgraph_core::color::AP1_LUMA;
use rawgraph_core::ImageBuf;

/// Number of bins.
pub const BINS: usize = 256;
/// Mid grey of the perceptual axis (the same constant as the curve).
const GREY: f32 = 0.18;
/// At most this many pixels are looked at (a stride over the picture keeps the cost small).
const MAX_SAMPLES: usize = 1_500_000;

/// Bin of a scene-linear luminance on the perceptual axis. Negative, NaN and infinite values
/// are clamped (nothing is lost, nothing panics).
pub fn bin_of(y: f32) -> usize {
    let y = if y.is_nan() { 0.0 } else { y.max(0.0) };
    let t = if y.is_infinite() { 1.0 } else { y / (y + GREY) };
    ((t * BINS as f32) as usize).min(BINS - 1)
}

/// Counts of the luminance of `img` per bin of the perceptual axis.
pub fn luminance_counts(img: &ImageBuf) -> [u32; BINS] {
    let mut counts = [0u32; BINS];
    let pixels = img.width() as usize * img.height() as usize;
    if pixels == 0 {
        return counts;
    }
    let stride = pixels.div_ceil(MAX_SAMPLES).max(1);
    for px in img.data().as_chunks::<4>().0.iter().step_by(stride) {
        let y = AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2];
        counts[bin_of(y)] += 1;
    }
    counts
}

/// Heights 0..1: the counts divided by their 99th percentile (of the bins), so one spike is
/// cut off at full height instead of squashing everything else. An empty histogram stays flat.
pub fn scale_to_percentile(counts: &[u32; BINS]) -> Vec<f32> {
    let mut sorted: Vec<u32> = counts.to_vec();
    sorted.sort_unstable();
    // 99th percentile of 256 bins: the bin at rank ceil(0.99 * 256) - 1 = 253.
    let rank = ((0.99 * BINS as f32).ceil() as usize).clamp(1, BINS) - 1;
    let p99 = sorted[rank].max(1) as f32;
    if sorted[BINS - 1] == 0 {
        return vec![0.0; BINS];
    }
    counts.iter().map(|c| (*c as f32 / p99).min(1.0)).collect()
}

/// The histogram of `img` ready to draw.
pub fn curve_histogram(img: &ImageBuf) -> Vec<f32> {
    scale_to_percentile(&luminance_counts(img))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grey_image(values: &[f32]) -> ImageBuf {
        let mut data = Vec::new();
        for v in values {
            data.extend_from_slice(&[*v, *v, *v, 1.0]);
        }
        ImageBuf::from_rgba(values.len() as u32, 1, data).unwrap()
    }

    #[test]
    fn bins_follow_the_perceptual_axis() {
        assert_eq!(bin_of(0.0), 0);
        assert_eq!(bin_of(0.18), 128, "mid grey is in the middle: t = 0.5");
        assert_eq!(bin_of(1.0), (1.0f32 / 1.18 * 256.0) as usize);
        assert_eq!(bin_of(1.0e9), 255);
        assert_eq!(bin_of(f32::INFINITY), 255);
        assert_eq!(bin_of(-3.0), 0, "negative values count as black");
        assert_eq!(bin_of(f32::NAN), 0);
        // Monotonic.
        let mut last = 0;
        for i in 0..2000 {
            let b = bin_of(i as f32 * 0.01);
            assert!(b >= last);
            last = b;
        }
    }

    #[test]
    fn counts_use_the_ap1_luminance() {
        // A pure green pixel is much brighter than a pure blue one of the same value.
        let img = ImageBuf::from_rgba(2, 1, vec![0.0, 1.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0]).unwrap();
        let c = luminance_counts(&img);
        assert_eq!(c.iter().sum::<u32>(), 2);
        assert_eq!(c[bin_of(0.674_081_8)], 1);
        assert_eq!(c[bin_of(0.053_689_5)], 1);
        assert_eq!(luminance_counts(&ImageBuf::new(0, 0)).iter().sum::<u32>(), 0);
    }

    #[test]
    fn one_spike_is_cut_at_the_99th_percentile_and_does_not_flatten_the_rest() {
        let mut counts = [100u32; BINS];
        counts[0] = 1_000_000; // a clipped-black spike
        counts[1] = 300; // and a second, smaller one
        counts[100] = 150;
        let h = scale_to_percentile(&counts);
        assert_eq!(h.len(), BINS);
        // Rank 253 of the sorted bins is 150: that is the full height.
        assert_eq!(h[0], 1.0, "the spike is cut off at full height");
        assert_eq!(h[1], 1.0);
        assert!((h[100] - 1.0).abs() < 1e-6, "{}", h[100]);
        assert!((h[5] - 100.0 / 150.0).abs() < 1e-6, "the others are not squashed to nothing: {}", h[5]);
        // Scaled by the maximum the spike would have flattened them to 0.0001.
        assert!(h[5] > 1000.0 * (100.0 / 1_000_000.0));
        // Empty and tiny cases.
        assert!(scale_to_percentile(&[0; BINS]).iter().all(|v| *v == 0.0));
        let mut one = [0u32; BINS];
        one[7] = 5;
        let h = scale_to_percentile(&one);
        assert_eq!(h[7], 1.0);
        assert_eq!(h[8], 0.0);
    }

    #[test]
    fn a_whole_picture_is_binned_and_scaled() {
        let img = grey_image(&[0.18; 50].iter().chain([0.0f32; 5].iter()).copied().collect::<Vec<_>>());
        let h = curve_histogram(&img);
        assert_eq!(h.len(), BINS);
        assert_eq!(h[126..130].iter().sum::<f32>(), 1.0, "all 50 mid-grey pixels share one bin");
        assert!(h[0] > 0.0 && h[0] <= 1.0);
        assert_eq!(h[200], 0.0);
    }
}
