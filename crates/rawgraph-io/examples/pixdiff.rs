//! pixdiff <a> <b>: largest and mean absolute difference between the 8-bit pixels of two images.
fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(a), Some(b)) = (args.next(), args.next()) else {
        eprintln!("usage: pixdiff <a> <b>");
        return;
    };
    let (Ok(a), Ok(b)) = (image::open(&a), image::open(&b)) else {
        eprintln!("cannot read both images");
        return;
    };
    let (a, b) = (a.to_rgb8(), b.to_rgb8());
    if a.dimensions() != b.dimensions() {
        println!("different sizes {:?} vs {:?}", a.dimensions(), b.dimensions());
        return;
    }
    let diffs: Vec<u32> = a.as_raw().iter().zip(b.as_raw()).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs()).collect();
    let max = diffs.iter().max().copied().unwrap_or(0);
    let mean = diffs.iter().map(|d| *d as f64).sum::<f64>() / diffs.len().max(1) as f64;
    let mean_a = a.as_raw().iter().map(|v| *v as f64).sum::<f64>() / a.as_raw().len().max(1) as f64;
    let mean_b = b.as_raw().iter().map(|v| *v as f64).sum::<f64>() / b.as_raw().len().max(1) as f64;
    println!("max diff {max}, mean diff {mean:.4}, mean value {mean_a:.2} vs {mean_b:.2}");
}
