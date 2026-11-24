//! AVIF writer: pure-Rust AV1 encoding with `ravif` (rav1e, no assembly, no external tools) and
//! container writing with `avif-serialize`.
//!
//! **Colour signalling.** `ravif` always labels its AV1 bitstream sRGB (CICP primaries 1,
//! transfer 13, matrix 6) and does not let the caller change that. So the AV1 payload is taken
//! out of its output, the colour description in the sequence header OBU is rewritten bit-exactly
//! for the other spaces, and the file is assembled again with `avif-serialize`, which writes the
//! matching `colr`/`nclx` box. Container and bitstream therefore agree. No ICC profile is
//! embedded (the container writer has no support for a `prof` box), so Adobe RGB (1998), which
//! has no CICP code, is refused with [`IoError::Unsupported`]. For sRGB the container carries no
//! `colr` box (the writer omits it for the sRGB default; the bitstream says sRGB).
//!
//! | space        | colour primaries (CICP) | transfer (CICP)          |
//! |--------------|-------------------------|--------------------------|
//! | sRGB         | 1 (BT.709 = sRGB)       | 13 (sRGB)                |
//! | Display P3   | 12 (SMPTE EG 432-1)     | 13 (sRGB)                |
//! | Rec. 2020    | 9 (BT.2020)             | 14 (BT.2020 10-bit)      |
//!
//! Rec. 2020 note: the pixels use a pure 2.4 power law; CICP has no code for that (14 is the
//! BT.2020 OETF, whose toe differs from a power law below about 2 % linear light). Code 14 is the
//! conventional label for SDR Rec. 2020 content that is displayed with BT.1886 (gamma 2.4), which
//! is what the pixels are made for; the pixel values are not altered.
//!
//! **EXIF** is supported: the TIFF stream is stored as the `Exif` item of the file.

use avif_serialize::constants::{ColorPrimaries, MatrixCoefficients, TransferCharacteristics};
use avif_serialize::Aviffy;
use rawgraph_core::color::OutputSpace;
use ravif::{Encoder, Img, RGB8};

use crate::meta::exif_blob;
use crate::{IoError, SaveOptions};

/// rav1e speed preset (1 slowest .. 10 fastest). Measured once: a synthetic 6000x4000 image (gradients
/// plus noise) at quality 80 took 7.5 s with `cargo run --release` on 6 threads (1.7 MB); photographs
/// with more detail take longer.
const SPEED: u8 = 6;
/// ravif encodes 8-bit input with 10-bit internal precision (its `BitDepth::Auto`).
const AV1_DEPTH: u8 = 10;
/// ravif's YCbCr model uses BT.601 coefficients (CICP matrix 6).
const MATRIX_BT601: u8 = 6;

/// CICP (colour primaries, transfer characteristics) of an output space, or why there is none.
pub(crate) fn cicp(space: OutputSpace) -> Result<(u8, u8), IoError> {
    match space {
        OutputSpace::Srgb => Ok((1, 13)),
        OutputSpace::DisplayP3 => Ok((12, 13)),
        OutputSpace::Rec2020 => Ok((9, 14)),
        OutputSpace::AdobeRgb => Err(IoError::Unsupported(
            "AVIF cannot signal Adobe RGB (1998): it has no CICP colour-primaries code and the AVIF writer embeds no ICC profile; \
             export as JPEG/PNG/TIFF or choose sRGB, Display P3 or Rec. 2020"
                .into(),
        )),
    }
}

fn enc_err(what: impl std::fmt::Display) -> IoError {
    IoError::Io(format!("AVIF encoder: {what}"))
}

/// Encodes 8-bit RGB (already in `options.space`) to an AVIF file in memory.
pub(crate) fn encode(width: u32, height: u32, rgb8: &[u8], options: &SaveOptions) -> Result<Vec<u8>, IoError> {
    let (primaries, transfer) = cicp(options.space)?;
    let pixels: Vec<RGB8> = rgb8.as_chunks::<3>().0.iter().map(|c| RGB8::new(c[0], c[1], c[2])).collect();
    let img = Img::new(&pixels[..], width as usize, height as usize);
    let encoder = Encoder::new().with_quality(f32::from(options.jpeg_quality.clamp(1, 100))).with_speed(SPEED);
    let encoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| encoder.encode_rgb(img)))
        .map_err(|_| enc_err("panicked"))?
        .map_err(enc_err)?;

    // The AV1 payload is the whole mdat of ravif's (alpha-less, EXIF-less) file.
    let (start, end) = find_box(&encoded.avif_file, b"mdat").ok_or_else(|| enc_err("no mdat box"))?;
    let mut av1 = encoded.avif_file[start..end].to_vec();
    if av1.len() != encoded.color_byte_size {
        return Err(enc_err("unexpected container layout"));
    }
    if (primaries, transfer) != (1, 13) {
        relabel(&mut av1, primaries, transfer)?;
    }

    let mut serializer = Aviffy::new();
    serializer
        .set_color_primaries(match primaries {
            12 => ColorPrimaries::DisplayP3,
            9 => ColorPrimaries::Bt2020,
            _ => ColorPrimaries::Bt709,
        })
        .set_transfer_characteristics(if transfer == 14 { TransferCharacteristics::Bt2020_10 } else { TransferCharacteristics::Srgb })
        .set_matrix_coefficients(MatrixCoefficients::Bt601);
    if let Some(md) = &options.metadata {
        serializer.set_exif(exif_blob(md, options.space == OutputSpace::Srgb));
    }
    Ok(serializer.to_vec(&av1, None, width, height, AV1_DEPTH))
}

/// Rewrites the colour description of the sequence header OBU in `av1` (primaries, transfer;
/// matrix stays BT.601). Fails unless exactly one sRGB colour description is found.
fn relabel(av1: &mut [u8], primaries: u8, transfer: u8) -> Result<(), IoError> {
    let bad = |what: &str| enc_err(format!("cannot relabel colour space ({what})"));
    let (start, len) = sequence_header(av1).ok_or_else(|| bad("no sequence header"))?;
    let header = &mut av1[start..start + len];
    // color_description_present_flag, colour_primaries(8), transfer_characteristics(8),
    // matrix_coefficients(8), in this order and without gaps.
    let pattern = (1u32 << 24) | (1 << 16) | (13 << 8) | u32::from(MATRIX_BT601);
    let new = (1u32 << 24) | (u32::from(primaries) << 16) | (u32::from(transfer) << 8) | u32::from(MATRIX_BT601);
    let bits = header.len() * 8;
    let get = |h: &[u8], at: usize| -> u32 {
        (0..25).fold(0u32, |acc, i| {
            let p = at + i;
            (acc << 1) | u32::from((h[p / 8] >> (7 - p % 8)) & 1)
        })
    };
    let hits: Vec<usize> = (0..bits.saturating_sub(24)).filter(|&at| get(header, at) == pattern).collect();
    let [at] = hits[..] else { return Err(bad("sequence header colour description not found")) };
    for i in 0..25 {
        let p = at + i;
        let bit = ((new >> (24 - i)) & 1) as u8;
        header[p / 8] = (header[p / 8] & !(1 << (7 - p % 8))) | (bit << (7 - p % 8));
    }
    Ok(())
}

/// Payload range `(start, end)` of the first top-level box of the given type.
fn find_box(file: &[u8], kind: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = 0usize;
    while pos + 8 <= file.len() {
        let size = u32::from_be_bytes(file[pos..pos + 4].try_into().ok()?) as usize;
        let (header, size) = match size {
            0 => (8, file.len() - pos),
            1 => (16, u64::from_be_bytes(file.get(pos + 8..pos + 16)?.try_into().ok()?) as usize),
            n => (8, n),
        };
        if size < header || pos.checked_add(size)? > file.len() {
            return None;
        }
        if &file[pos + 4..pos + 8] == kind {
            return Some((pos + header, pos + size));
        }
        pos += size;
    }
    None
}

/// Offset and length of the payload of the first sequence-header OBU (type 1) in `obus`.
fn sequence_header(obus: &[u8]) -> Option<(usize, usize)> {
    let mut pos = 0usize;
    while pos < obus.len() {
        let head = obus[pos];
        let kind = (head >> 3) & 0xF;
        let has_extension = head & 4 != 0;
        let has_size = head & 2 != 0;
        pos += 1 + usize::from(has_extension);
        if !has_size {
            return None;
        }
        let (mut size, mut shift) = (0usize, 0u32);
        loop {
            let b = *obus.get(pos)?;
            pos += 1;
            size |= usize::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
            if shift > 28 {
                return None;
            }
        }
        if pos.checked_add(size)? > obus.len() {
            return None;
        }
        if kind == 1 {
            return Some((pos, size));
        }
        pos += size;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::new();
        for y in 0..h {
            for x in 0..w {
                v.extend_from_slice(&[(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, ((x ^ y) * 4 % 256) as u8]);
            }
        }
        v
    }

    fn opts(space: OutputSpace) -> SaveOptions {
        SaveOptions { space, jpeg_quality: 70, ..Default::default() }
    }

    /// Colour labels `(primaries, transfer, matrix)` of the container's `colr`/`nclx` box (the
    /// sRGB default is "no box": 1/13/6) and of the sequence header, read independently of `relabel`.
    fn labels(file: &[u8]) -> ((u16, u16, u16), (u8, u8, u8)) {
        let container = match file.windows(8).position(|w| w == b"colrnclx") {
            Some(i) => {
                let be = |o: usize| u16::from_be_bytes([file[i + 8 + o], file[i + 9 + o]]);
                (be(0), be(2), be(4))
            }
            None => (1, 13, 6),
        };
        let (s, e) = find_box(file, b"mdat").expect("mdat");
        let (start, len) = sequence_header(&file[s..e]).expect("sequence header");
        let h = &file[s + start..s + start + len];
        let bit = |p: usize| (h[p / 8] >> (7 - p % 8)) & 1;
        for at in 0..h.len() * 8 - 24 {
            let v = (0..25).fold(0u32, |a, i| (a << 1) | u32::from(bit(at + i)));
            if v >> 24 == 1 && (v >> 16) as u8 as u16 == container.0 && (v >> 8) as u8 as u16 == container.1 {
                return (container, ((v >> 16) as u8, (v >> 8) as u8, v as u8));
            }
        }
        panic!("sequence header does not carry the container's colour labels {container:?}");
    }

    #[test]
    fn signature_dimensions_and_exif() {
        let px = image(40, 24);
        let md = crate::ExportMetadata { make: Some("SONY".into()), ..Default::default() };
        let file = encode(40, 24, &px, &SaveOptions { metadata: Some(md), ..opts(OutputSpace::Srgb) }).expect("encode");
        assert_eq!(&file[4..12], b"ftypavif");
        let ispe = file.windows(4).position(|w| w == b"ispe").expect("ispe");
        let be = |o: usize| u32::from_be_bytes(file[ispe + o..ispe + o + 4].try_into().expect("4 bytes"));
        assert_eq!((be(8), be(12)), (40, 24));
        assert!(file.windows(4).any(|w| w == b"Exif"), "Exif item missing");
        assert!(file.windows(4).any(|w| w == b"SONY"), "make missing");
        let plain = encode(40, 24, &px, &opts(OutputSpace::Srgb)).expect("encode");
        assert!(!plain.windows(4).any(|w| w == b"Exif"));
        assert_eq!(labels(&plain).1, (1, 13, 6));
    }

    #[test]
    fn colour_labels_follow_the_space_in_container_and_bitstream() {
        let px = image(33, 17);
        for (space, want) in [(OutputSpace::Srgb, (1u8, 13u8)), (OutputSpace::DisplayP3, (12, 13)), (OutputSpace::Rec2020, (9, 14))] {
            let file = encode(33, 17, &px, &opts(space)).expect("encode");
            let ((cp, tc, m), (scp, stc, smc)) = labels(&file);
            assert_eq!(((cp as u8, tc as u8), (scp, stc)), (want, want), "{space:?}");
            assert_eq!((m, smc), (6, 6), "matrix coefficients");
        }
    }

    #[test]
    fn adobe_rgb_is_refused_with_a_clear_error() {
        let err = encode(8, 8, &image(8, 8), &opts(OutputSpace::AdobeRgb)).expect_err("must fail");
        let IoError::Unsupported(msg) = err else { panic!("wrong error kind") };
        assert!(msg.contains("Adobe RGB"), "{msg}");
    }

    #[test]
    fn relabel_rejects_foreign_data() {
        assert!(relabel(&mut [0u8; 64], 12, 13).is_err());
        assert!(relabel(&mut [], 12, 13).is_err());
        let mut obus = vec![0x0A, 0x00];
        assert!(relabel(&mut obus, 12, 13).is_err());
    }
}
