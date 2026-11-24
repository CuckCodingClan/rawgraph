//! Compares two PNG files pixel by pixel: `pngdiff a.png b.png`.
//! Prints the maximum difference in code values and the share of differing pixels.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [a, b] = args.as_slice() else {
        eprintln!("usage: pngdiff <a.png> <b.png>");
        std::process::exit(2);
    };
    let load = |p: &str| match image::open(p) {
        Ok(i) => i.to_rgba8(),
        Err(e) => {
            eprintln!("{p}: {e}");
            std::process::exit(2);
        }
    };
    let (ia, ib) = (load(a), load(b));
    if ia.dimensions() != ib.dimensions() {
        println!("size differs: {:?} vs {:?}", ia.dimensions(), ib.dimensions());
        std::process::exit(1);
    }
    let (mut max, mut differing, mut sum) = (0u8, 0usize, 0u64);
    for (pa, pb) in ia.pixels().zip(ib.pixels()) {
        let d = pa.0.iter().zip(&pb.0).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        max = max.max(d);
        sum += u64::from(d);
        differing += usize::from(d > 0);
    }
    let n = ia.pixels().len().max(1);
    println!(
        "{}x{}  max difference {} code values, {:.4} % of pixels differ, mean per-pixel max {:.5}",
        ia.width(),
        ia.height(),
        max,
        differing as f64 * 100.0 / n as f64,
        sum as f64 / n as f64
    );
}
