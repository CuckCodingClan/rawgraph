//! ICC profiles in WebP: the encoder (`webp-rust`) cannot write an `ICCP` chunk, so the finished
//! RIFF container is rewritten into the extended format (`VP8X` first, then `ICCP`, then the
//! image chunks and metadata) as described in the WebP container specification.

use crate::IoError;

const ICC_FLAG: u8 = 0x20;
const ALPHA_FLAG: u8 = 0x10;

struct Chunk<'a> {
    tag: [u8; 4],
    data: &'a [u8],
}

fn bad(why: &str) -> IoError {
    IoError::Io(format!("WebP container: {why}"))
}

fn parse(bytes: &[u8]) -> Result<Vec<Chunk<'_>>, IoError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(bad("not a RIFF/WEBP file"));
    }
    let riff = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let end = riff.checked_add(8).ok_or_else(|| bad("size overflow"))?.min(bytes.len());
    let mut at = 12usize;
    let mut out = Vec::new();
    while at + 8 <= end {
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]) as usize;
        let data = bytes.get(at + 8..at + 8 + size).ok_or_else(|| bad("truncated chunk"))?;
        out.push(Chunk { tag: [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]], data });
        at += 8 + size + (size & 1);
    }
    if out.is_empty() {
        return Err(bad("no chunks"));
    }
    Ok(out)
}

fn push_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() & 1 == 1 {
        out.push(0);
    }
}

fn u24(v: u32) -> [u8; 3] {
    [v as u8, (v >> 8) as u8, (v >> 16) as u8]
}

/// Returns `webp` with `icc` stored as its `ICCP` chunk (an existing `ICCP` is replaced). A simple
/// file (`VP8 ` / `VP8L` only) is converted to the extended format first. `width` / `height`
/// are the canvas size, used when a `VP8X` header has to be created.
pub(crate) fn with_icc(webp: &[u8], icc: &[u8], width: u32, height: u32) -> Result<Vec<u8>, IoError> {
    if icc.is_empty() || icc.len() > (1 << 24) {
        return Err(bad("unusable ICC profile"));
    }
    let chunks = parse(webp)?;
    let mut alpha = false;
    let mut vp8x_header = None;
    for c in &chunks {
        match &c.tag {
            b"VP8X" if c.data.len() == 10 => vp8x_header = Some(c.data),
            b"ALPH" => alpha = true,
            // VP8L header: signature 0x2f, 14+14 bits of size, then the alpha_is_used bit.
            b"VP8L" if c.data.len() >= 5 && c.data[0] == 0x2f => alpha |= c.data[4] & 0x10 != 0,
            _ => {}
        }
    }
    let header = match vp8x_header {
        Some(h) => {
            let mut h = h.to_vec();
            h[0] |= ICC_FLAG;
            h
        }
        None => {
            let mut h = vec![ICC_FLAG | if alpha { ALPHA_FLAG } else { 0 }, 0, 0, 0];
            h.extend_from_slice(&u24(width.saturating_sub(1)));
            h.extend_from_slice(&u24(height.saturating_sub(1)));
            h
        }
    };
    let mut body = Vec::with_capacity(webp.len() + icc.len() + 32);
    body.extend_from_slice(b"WEBP");
    push_chunk(&mut body, b"VP8X", &header);
    push_chunk(&mut body, b"ICCP", icc);
    for c in chunks.iter().filter(|c| !matches!(&c.tag, b"VP8X" | b"ICCP")) {
        push_chunk(&mut body, &c.tag, c.data);
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// The payload of the `ICCP` chunk, if the file has one.
pub fn webp_icc_profile(webp: &[u8]) -> Option<Vec<u8>> {
    parse(webp).ok()?.into_iter().find(|c| &c.tag == b"ICCP").map(|c| c.data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        push_chunk(&mut body, tag, data);
        let mut f = b"RIFF".to_vec();
        f.extend_from_slice(&(body.len() as u32).to_le_bytes());
        f.extend_from_slice(&body);
        f
    }

    #[test]
    fn simple_file_becomes_extended_with_icc_first() {
        let lossless = simple(b"VP8L", &[0x2f, 0, 0, 0, 0x10, 1, 2, 3]);
        let out = with_icc(&lossless, &[9, 8, 7], 640, 480).unwrap();
        let chunks = parse(&out).unwrap();
        let tags: Vec<_> = chunks.iter().map(|c| c.tag).collect();
        assert_eq!(tags, [*b"VP8X", *b"ICCP", *b"VP8L"]);
        assert_eq!(chunks[0].data[0], ICC_FLAG | ALPHA_FLAG, "alpha bit comes from the VP8L header");
        assert_eq!(&chunks[0].data[4..7], &u24(639));
        assert_eq!(&chunks[0].data[7..10], &u24(479));
        assert_eq!(webp_icc_profile(&out).as_deref(), Some(&[9u8, 8, 7][..]));
        assert_eq!(out.len() % 2, 0);
    }

    #[test]
    fn existing_extended_header_keeps_its_flags_and_chunk_order() {
        let mut body = b"WEBP".to_vec();
        push_chunk(&mut body, b"VP8X", &[0x08, 0, 0, 0, 1, 0, 0, 1, 0, 0]);
        push_chunk(&mut body, b"VP8 ", &[1, 2, 3]);
        push_chunk(&mut body, b"EXIF", &[4, 5]);
        let mut f = b"RIFF".to_vec();
        f.extend_from_slice(&(body.len() as u32).to_le_bytes());
        f.extend_from_slice(&body);
        let out = with_icc(&f, &[1; 5], 2, 2).unwrap();
        let chunks = parse(&out).unwrap();
        assert_eq!(chunks.iter().map(|c| c.tag).collect::<Vec<_>>(), [*b"VP8X", *b"ICCP", *b"VP8 ", *b"EXIF"]);
        assert_eq!(chunks[0].data[0], 0x08 | ICC_FLAG, "the EXIF flag stays");
        assert_eq!(chunks[1].data.len(), 5);
        // A second pass replaces the profile instead of duplicating it.
        let again = with_icc(&out, &[2; 3], 2, 2).unwrap();
        assert_eq!(parse(&again).unwrap().iter().filter(|c| &c.tag == b"ICCP").count(), 1);
        assert_eq!(webp_icc_profile(&again).as_deref(), Some(&[2u8; 3][..]));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(with_icc(b"RIFF", &[1], 1, 1).is_err());
        assert!(with_icc(&simple(b"VP8L", &[0x2f, 0, 0, 0, 0]), &[], 1, 1).is_err());
        let mut cut = simple(b"VP8L", &[0x2f; 40]);
        cut.truncate(30);
        assert!(with_icc(&cut, &[1], 1, 1).is_err());
    }
}
