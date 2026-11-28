//! XMP packets with the keywords of an export and the code that embeds a packet into encoded
//! JPEG, PNG and WebP files (TIFF is handled in `meta`, where its IFD is rebuilt).
//!
//! Only `dc:subject` and `lr:hierarchicalSubject` are written (see `rawgraph_core::keywords`).
//! No function panics on file content: the input is the encoder's own output, and every length is
//! checked.

use rawgraph_core::keywords::{normalize_keywords, xmp_keyword_elements, XMP_KEYWORD_NAMESPACES};

use crate::IoError;

/// A JPEG APP1 segment holds at most 65,533 bytes; the namespace header takes 29.
const JPEG_MAX_PACKET: usize = 65_000;
/// Larger packets are not written into PNG, WebP and TIFF either.
const MAX_PACKET: usize = 16 << 20;

fn packet_text(list: &[String]) -> String {
    let body = xmp_keyword_elements(list);
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\"{XMP_KEYWORD_NAMESPACES}>{body}</rdf:Description></rdf:RDF></x:xmpmeta>\n<?xpacket end=\"w\"?>"
    )
}

/// The longest prefix of the normalised (sorted) keyword list whose packet fits `max_len` bytes,
/// i.e. keywords dropped from the end one at a time until it fits (found by bisection: the packet
/// only grows with the list).
fn fitting_prefix(keywords: &[String], max_len: usize) -> Vec<String> {
    let list = normalize_keywords(keywords);
    if list.is_empty() || packet_text(&list).len() <= max_len {
        return list;
    }
    let (mut lo, mut hi) = (0usize, list.len()); // `lo` fits (the empty list does), `hi` does not
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if packet_text(&list[..mid]).len() <= max_len {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    list[..lo].to_vec()
}

/// The XMP packet of `keywords` and the number of keywords left out, or `None` when there is
/// nothing to write (or not even one keyword fits).
pub(crate) fn keyword_packet(keywords: &[String], max_len: usize) -> Option<(Vec<u8>, usize)> {
    let total = normalize_keywords(keywords).len();
    let kept = fitting_prefix(keywords, max_len);
    if kept.is_empty() {
        return None;
    }
    Some((packet_text(&kept).into_bytes(), total - kept.len()))
}

/// The keywords of a list that fit into a JPEG XMP segment (a prefix of the normalised list); the
/// caller reports `list.len() - result.len()` as not embedded.
pub fn jpeg_embeddable_keywords(keywords: &[String]) -> Vec<String> {
    fitting_prefix(keywords, JPEG_MAX_PACKET)
}

fn bad(what: &str) -> IoError {
    IoError::Io(format!("cannot add the XMP packet: {what}"))
}

/// Inserts the packet as an APP1 segment after the leading APPn segments (JFIF, Exif, ICC), which
/// keeps the Exif segment where readers expect it.
pub(crate) fn embed_jpeg(bytes: Vec<u8>, keywords: &[String]) -> Result<Vec<u8>, IoError> {
    let Some((packet, _)) = keyword_packet(keywords, JPEG_MAX_PACKET) else { return Ok(bytes) };
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Err(bad("not a JPEG stream"));
    }
    const NS: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    let mut pos = 2;
    while pos + 4 <= bytes.len() && bytes[pos] == 0xFF && (0xE0..=0xEF).contains(&bytes[pos + 1]) {
        let len = usize::from(u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]));
        if len < 2 || pos + 2 + len > bytes.len() {
            return Err(bad("damaged JPEG segment"));
        }
        pos += 2 + len;
    }
    let seg_len = u16::try_from(2 + NS.len() + packet.len()).map_err(|_| bad("packet too large"))?;
    let mut out = Vec::with_capacity(bytes.len() + usize::from(seg_len) + 2);
    out.extend_from_slice(&bytes[..pos]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&seg_len.to_be_bytes());
    out.extend_from_slice(NS);
    out.extend_from_slice(&packet);
    out.extend_from_slice(&bytes[pos..]);
    Ok(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

/// Inserts an uncompressed iTXt chunk `XML:com.adobe.xmp` before the first IDAT chunk.
pub(crate) fn embed_png(bytes: Vec<u8>, keywords: &[String]) -> Result<Vec<u8>, IoError> {
    let Some((packet, _)) = keyword_packet(keywords, MAX_PACKET) else { return Ok(bytes) };
    if bytes.len() < 8 || bytes[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Err(bad("not a PNG stream"));
    }
    let mut pos = 8;
    loop {
        if pos + 8 > bytes.len() {
            return Err(bad("no IDAT chunk"));
        }
        let len = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        if &bytes[pos + 4..pos + 8] == b"IDAT" {
            break;
        }
        pos = pos.checked_add(12 + len).ok_or_else(|| bad("damaged PNG chunk"))?;
    }
    // keyword NUL, compression flag 0, method 0, language "" NUL, translated keyword "" NUL, text
    let mut data = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
    data.extend_from_slice(&packet);
    let mut chunk = Vec::with_capacity(data.len() + 12);
    chunk.extend_from_slice(&u32::try_from(data.len()).map_err(|_| bad("packet too large"))?.to_be_bytes());
    chunk.extend_from_slice(b"iTXt");
    chunk.extend_from_slice(&data);
    let crc = crc32(&chunk[4..]);
    chunk.extend_from_slice(&crc.to_be_bytes());
    let mut out = Vec::with_capacity(bytes.len() + chunk.len());
    out.extend_from_slice(&bytes[..pos]);
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&bytes[pos..]);
    Ok(out)
}

/// Appends an `XMP ` chunk to a WebP file and sets the XMP flag of its `VP8X` header (one is created
/// in front of a simple-format file; `width` x `height` is the canvas).
pub(crate) fn embed_webp(bytes: Vec<u8>, keywords: &[String], width: u32, height: u32) -> Result<Vec<u8>, IoError> {
    let Some((packet, _)) = keyword_packet(keywords, MAX_PACKET) else { return Ok(bytes) };
    if bytes.len() < 20 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(bad("not a WebP stream"));
    }
    let mut body = bytes[12..].to_vec();
    if &body[..4] == b"VP8X" {
        if body.len() < 18 {
            return Err(bad("damaged VP8X chunk"));
        }
        body[8] |= 0x04; // XMP metadata present
    } else {
        let (w, h) = (width.saturating_sub(1), height.saturating_sub(1));
        if w >= 1 << 24 || h >= 1 << 24 {
            return Err(bad("canvas too large"));
        }
        // A simple lossless stream says in its header whether it uses alpha; the extended header must too.
        let alpha = &body[..4] == b"VP8L" && body.len() >= 13 && body[8] == 0x2f && body[12] & 0x10 != 0;
        let mut head = Vec::with_capacity(18 + body.len());
        head.extend_from_slice(b"VP8X");
        head.extend_from_slice(&10u32.to_le_bytes());
        head.extend_from_slice(&[0x04 | if alpha { 0x10 } else { 0 }, 0, 0, 0]);
        head.extend_from_slice(&w.to_le_bytes()[..3]);
        head.extend_from_slice(&h.to_le_bytes()[..3]);
        head.extend_from_slice(&body);
        body = head;
    }
    body.extend_from_slice(b"XMP ");
    body.extend_from_slice(&u32::try_from(packet.len()).map_err(|_| bad("packet too large"))?.to_le_bytes());
    body.extend_from_slice(&packet);
    if packet.len() % 2 == 1 {
        body.push(0);
    }
    let riff = u32::try_from(body.len() + 4).map_err(|_| bad("file too large"))?;
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    Ok(out)
}

/// The packet of an export for the TIFF tag 700, or `None` without keywords.
pub(crate) fn tiff_packet(keywords: &[String]) -> Option<Vec<u8>> {
    keyword_packet(keywords, MAX_PACKET).map(|(p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn many(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("keyword number {i:05} with some padding to make the packet grow quickly")).collect()
    }

    #[test]
    fn a_huge_list_keeps_the_longest_prefix_that_fits() {
        let list = many(2000);
        let kept = jpeg_embeddable_keywords(&list);
        assert!(!kept.is_empty() && kept.len() < list.len());
        assert_eq!(kept, normalize_keywords(&list)[..kept.len()], "keywords are dropped from the end");
        assert!(packet_text(&kept).len() <= JPEG_MAX_PACKET);
        let one_more = &normalize_keywords(&list)[..kept.len() + 1];
        assert!(packet_text(one_more).len() > JPEG_MAX_PACKET, "exactly one more would not fit: not halved");
        let (packet, dropped) = keyword_packet(&list, JPEG_MAX_PACKET).unwrap_or_default();
        assert_eq!(dropped, list.len() - kept.len());
        assert!(packet.len() <= JPEG_MAX_PACKET);
    }

    #[test]
    fn a_small_list_is_kept_whole() {
        let list = many(3);
        assert_eq!(jpeg_embeddable_keywords(&list).len(), 3);
        assert_eq!(keyword_packet(&list, JPEG_MAX_PACKET).map(|(_, d)| d), Some(0));
        assert!(keyword_packet(&[], JPEG_MAX_PACKET).is_none());
    }
}
