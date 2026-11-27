//! Whole-image pixel permutation in column-major output order.
use rawgraph_core::{color::AP1_LUMA, ImageBuf};
use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMetric { Brightness, Saturation, Hue }

// f64 avoids overflow for finite f32 HDR inputs. Saturation is chroma divided
// by max absolute RGB, defined for signed values and zero for black.
fn key(p: &[f32; 4], metric: SortMetric) -> Option<f64> {
    if !p[..3].iter().all(|v| v.is_finite()) { return None; }
    let [r, g, b] = [p[0] as f64, p[1] as f64, p[2] as f64];
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;
    Some(match metric {
        SortMetric::Brightness => AP1_LUMA[0] as f64*r + AP1_LUMA[1] as f64*g + AP1_LUMA[2] as f64*b,
        SortMetric::Saturation => { let scale = r.abs().max(g.abs()).max(b.abs()); if scale == 0.0 { 0.0 } else { chroma / scale } },
        SortMetric::Hue => {
            if chroma == 0.0 { 0.0 } else {
                let a = r - (g+b)*0.5;
                let b = (g-b) * (3.0_f64.sqrt()*0.5);
                b.atan2(a).to_degrees().rem_euclid(360.0)
            }
        }
    })
}

/// Sort all pixels stably, reading ties in row-major source order and writing
/// top-to-bottom in each column, left-to-right. Non-finite RGB sorts last in
/// either direction, retaining source order. Alpha never affects sorting.
pub fn pixel_sort(img: &mut ImageBuf, metric: SortMetric, reverse: bool) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 { return; }
    let mut pixels: Vec<_> = img.data().par_chunks_exact(4).map(|c| {
        let p = [c[0], c[1], c[2], c[3]];
        (key(&p, metric), p)
    }).collect();
    pixels.par_sort_by(|a, b| match (a.0, b.0) {
        (Some(a), Some(b)) => if reverse { b.partial_cmp(&a).unwrap() } else { a.partial_cmp(&b).unwrap() },
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    // Each output row is contiguous; sorted position for (x,y) is x*h+y.
    img.data_mut().par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for (x, dst) in row.chunks_exact_mut(4).enumerate() {
            dst.copy_from_slice(&pixels[x * h + y].1);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_serial_reference_bitwise() {
        let (w, h) = (127usize, 83usize);
        let mut state = 17u32;
        let mut source = Vec::new();
        for i in 0..w*h {
            let mut p = [0.0; 4];
            for channel in &mut p {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                *channel = ((state >> 24) as f32 - 128.0) / 16.0;
            }
            if i % 19 == 0 { p = [1.0, 1.0, 1.0, i as f32]; }
            if i % 101 == 0 { p[0] = f32::from_bits(0x7fc00000 + i as u32); }
            if i % 103 == 0 { p[1] = f32::INFINITY; }
            if i % 107 == 0 { p[2] = f32::MAX; }
            source.extend_from_slice(&p);
        }
        for metric in [SortMetric::Brightness, SortMetric::Saturation, SortMetric::Hue] {
            for reverse in [false, true] {
                let mut reference: Vec<_> = source.chunks_exact(4).map(|c| {
                    let p = [c[0], c[1], c[2], c[3]];
                    (key(&p, metric), p)
                }).collect();
                reference.sort_by(|a,b| match (a.0,b.0) {
                    (Some(a),Some(b)) => if reverse { b.partial_cmp(&a).unwrap() } else { a.partial_cmp(&b).unwrap() },
                    (Some(_),None) => std::cmp::Ordering::Less,
                    (None,Some(_)) => std::cmp::Ordering::Greater,
                    (None,None) => std::cmp::Ordering::Equal,
                });
                let mut image = ImageBuf::from_rgba(w as u32,h as u32,source.clone()).unwrap();
                pixel_sort(&mut image, metric, reverse);
                for y in 0..h { for x in 0..w { for c in 0..4 {
                    assert_eq!(image.data()[(y*w+x)*4+c].to_bits(), reference[x*h+y].1[c].to_bits(), "{metric:?} reverse={reverse} x={x} y={y} c={c}");
                } } }
            }
        }
    }
    #[test]
    fn whole_image_column_order_and_alpha() {
        let mut im = ImageBuf::from_rgba(2, 2, vec![3.,3.,3.,0.3, 1.,1.,1.,0.1, 4.,4.,4.,0.4, -2.,-2.,-2.,0.2]).unwrap();
        pixel_sort(&mut im, SortMetric::Brightness, false);
        assert_eq!(im.pixel(0,0), [-2.,-2.,-2.,0.2]);
        assert_eq!(im.pixel(0,1), [1.,1.,1.,0.1]);
        assert_eq!(im.pixel(1,0), [3.,3.,3.,0.3]);
        assert_eq!(im.pixel(1,1), [4.,4.,4.,0.4]);
    }
    #[test]
    fn modes_ties_reverse_and_nonfinite() {
        for mode in [SortMetric::Brightness, SortMetric::Saturation, SortMetric::Hue] {
            let source = vec![1.,0.,0.,1., 1.,0.,0.,2., 0.,1.,0.,3., f32::NAN,0.,0.,4.];
            for reverse in [false, true] {
                let mut im = ImageBuf::from_rgba(4,1,source.clone()).unwrap();
                pixel_sort(&mut im, mode, reverse);
                let alphas: Vec<_> = im.data().chunks_exact(4).map(|p| p[3] as u32).collect();
                assert!(alphas.iter().position(|&a| a == 1) < alphas.iter().position(|&a| a == 2));
                assert_eq!(alphas[3], 4);
                let mut sorted = alphas; sorted.sort(); assert_eq!(sorted, [1,2,3,4]);
            }
        }
        let mut im = ImageBuf::from_rgba(3,1,vec![0.,0.,1.,1., 1.,0.,0.,2., 0.,1.,0.,3.]).unwrap();
        pixel_sort(&mut im, SortMetric::Hue, false);
        assert_eq!([im.pixel(0,0)[3],im.pixel(1,0)[3],im.pixel(2,0)[3]], [2.,3.,1.]);
        let mut im = ImageBuf::from_rgba(3,1,vec![0.5,0.5,0.5,1., 2.,0.,0.,2., 1.,0.5,0.5,3.]).unwrap();
        pixel_sort(&mut im, SortMetric::Saturation, false);
        assert_eq!([im.pixel(0,0)[3],im.pixel(1,0)[3],im.pixel(2,0)[3]], [1.,3.,2.]);
    }
}

#[cfg(test)]
mod speed_probe {
    use super::*;
    #[test] #[ignore]
    fn bench() {
        let (w,h)=(1600u32,1000u32);
        let mut seed=12345u32;
        let mut data=Vec::with_capacity((w*h*4) as usize);
        for _ in 0..w*h { for _ in 0..4 { seed=seed.wrapping_mul(1664525).wrapping_add(1013904223); data.push((seed >> 8) as f32 / 16777216.0); } }
        for mode in [SortMetric::Brightness, SortMetric::Hue] { for _ in 0..3 { let mut image=ImageBuf::from_rgba(w,h,data.clone()).unwrap(); let t=std::time::Instant::now(); pixel_sort(&mut image,mode,false); println!("{:?}: {:.3} ms",mode,t.elapsed().as_secs_f64()*1000.0); std::hint::black_box(image); } }
    }
}
