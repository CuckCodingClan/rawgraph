//! Colour balance: ASC-CDL style slope / offset / power per channel.

use rawgraph_core::{CdlParams, ImageBuf};
use rayon::prelude::*;

use crate::util::finite_or;

/// Powers are limited to this range (a power of 0 or a negative power is meaningless for a grade,
/// and a huge power overflows `f32`).
const POWER_RANGE: (f32, f32) = (0.01, 20.0);

/// ASC-CDL style colour balance, per channel: positive `x` -> `(x * slope + offset)^power`;
/// values that are `<= 0` after slope and offset are left at that linear value (no power
/// applied, so negative scene values survive and the curve is continuous at 0 for power 1).
///
/// The three channels are fully independent (no luminance coupling), alpha is untouched, and
/// [`CdlParams::IDENTITY`] returns immediately, bit-identical. Non-finite parameters count as
/// their neutral value; the power is limited to 0.01..20 and a result that would overflow `f32`
/// is limited to `f32::MAX`.
///
/// HDR behaviour: this is the CDL definition applied to the scene-referred linear value. The
/// power acts on the unbounded positive value with its pivot at 1.0, so highlights above 1.0
/// expand for `power > 1` and compress for `power < 1` (values below 1.0 do the opposite),
/// middle grey 0.18 moves much more than a highlight of 4.0 does in relative terms. Use the
/// slope for a gain-like grade of the whole range and the offset for lifting shadows.
pub fn color_wheels(img: &mut ImageBuf, p: &CdlParams) {
    if p.is_identity() || img.width() == 0 || img.height() == 0 {
        return;
    }
    let mut slope = [1.0f32; 3];
    let mut offset = [0.0f32; 3];
    let mut power = [1.0f32; 3];
    for c in 0..3 {
        slope[c] = finite_or(p.slope[c], 1.0);
        offset[c] = finite_or(p.offset[c], 0.0);
        power[c] = finite_or(p.power[c], 1.0).clamp(POWER_RANGE.0, POWER_RANGE.1);
    }
    let channel = move |c: usize, x: f32| -> f32 {
        let v = x * slope[c] + offset[c];
        if v > 0.0 {
            if power[c] == 1.0 {
                v
            } else {
                v.powf(power[c]).min(f32::MAX)
            }
        } else {
            v
        }
    };
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        px[0] = channel(0, px[0]);
        px[1] = channel(1, px[1]);
        px[2] = channel(2, px[2]);
    });
}
