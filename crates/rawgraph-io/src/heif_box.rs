//! Minimal HEIF (ISO/IEC 23008-12) box editing for files written by the operating system's
//! encoder: new item properties are appended to `ipco`, associated with the primary item in
//! `ipma`, and every `iloc` file offset behind the grown `meta` box is moved accordingly. Nothing
//! else in the file is touched. No panics: every malformed input is an `Err`.

/// A property box to add to the primary item.
pub(crate) struct NewProp {
    /// The complete box (header included).
    pub data: Vec<u8>,
    /// Transformative properties (`clap`, `irot`) must be marked essential.
    pub essential: bool,
}

/// `colr` box of type `nclx`: colour primaries, transfer, matrix (CICP codes), full range.
pub(crate) fn nclx_box(primaries: u16, transfer: u16, matrix: u16, full_range: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(19);
    b.extend_from_slice(&19u32.to_be_bytes());
    b.extend_from_slice(b"colrnclx");
    for v in [primaries, transfer, matrix] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.push(if full_range { 0x80 } else { 0 });
    b
}

/// `clap` box: the visible `width` x `height` window centred in a `canvas` picture.
pub(crate) fn clap_box(width: u32, height: u32, canvas: (u32, u32)) -> Vec<u8> {
    let mut b = Vec::with_capacity(40);
    b.extend_from_slice(&40u32.to_be_bytes());
    b.extend_from_slice(b"clap");
    // width, height as n/1; horizontal and vertical offset of the window centre from the canvas
    // centre as n/2 (a window narrower than the canvas by `d` pixels sits at -d/2... = 0 when
    // the crop is taken from the bottom/right, so the centre shifts by (width - canvas) / 2).
    let off = |w: u32, c: u32| (i64::from(w) - i64::from(c)) as i32;
    for (n, d) in [(width as i32, 1u32), (height as i32, 1), (off(width, canvas.0), 2), (off(height, canvas.1), 2)] {
        b.extend_from_slice(&n.to_be_bytes());
        b.extend_from_slice(&d.to_be_bytes());
    }
    b
}

struct BoxAt {
    kind: [u8; 4],
    start: usize,
    /// First byte after the 8-byte header.
    body: usize,
    end: usize,
}

fn boxes(d: &[u8], from: usize, to: usize) -> Result<Vec<BoxAt>, String> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let size = u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]) as usize;
        let end = if size == 0 { to } else { at.checked_add(size).ok_or("box size overflow")? };
        if size == 1 || (size != 0 && size < 8) || end > to {
            return Err(format!("unsupported or broken box at {at}"));
        }
        out.push(BoxAt { kind: [d[at + 4], d[at + 5], d[at + 6], d[at + 7]], start: at, body: at + 8, end });
        at = end;
    }
    Ok(out)
}

fn find<'a>(list: &'a [BoxAt], kind: &[u8; 4]) -> Result<&'a BoxAt, String> {
    list.iter().find(|b| &b.kind == kind).ok_or_else(|| format!("no {} box", String::from_utf8_lossy(kind)))
}

fn be(d: &[u8], at: usize, n: usize) -> Result<u64, String> {
    let s = d.get(at..at + n).ok_or("truncated box")?;
    Ok(s.iter().fold(0u64, |a, b| (a << 8) | u64::from(*b)))
}

enum Event {
    Insert(usize, Vec<u8>),
    Patch(usize, usize, u64),
}

impl Event {
    fn pos(&self) -> usize {
        match self {
            Event::Insert(p, _) | Event::Patch(p, ..) => *p,
        }
    }
}

/// Adds `props` to the primary item. See the module text.
pub(crate) fn add_primary_properties(file: &[u8], props: &[NewProp]) -> Result<Vec<u8>, String> {
    let top = boxes(file, 0, file.len())?;
    let meta = find(&top, b"meta")?;
    if meta.end - meta.body < 4 {
        return Err("meta too short".into());
    }
    let children = boxes(file, meta.body + 4, meta.end)?;
    let pitm = find(&children, b"pitm")?;
    let version = *file.get(pitm.body).ok_or("truncated pitm")?;
    let primary = be(file, pitm.body + 4, if version == 0 { 2 } else { 4 })?;
    let iprp = find(&children, b"iprp")?;
    let inner = boxes(file, iprp.body, iprp.end)?;
    let ipco = find(&inner, b"ipco")?;
    let ipma = find(&inner, b"ipma")?;
    let existing = boxes(file, ipco.body, ipco.end)?.len();
    let first_index = existing + 1;

    // Associations in ipma.
    let (ipma_version, ipma_flags) = (be(file, ipma.body, 1)?, be(file, ipma.body + 1, 3)?);
    let wide = ipma_flags & 1 == 1;
    let id_len = if ipma_version < 1 { 2 } else { 4 };
    let count = be(file, ipma.body + 4, 4)? as usize;
    let mut at = ipma.body + 8;
    let mut entry = None;
    for _ in 0..count {
        let id = be(file, at, id_len)?;
        let n_at = at + id_len;
        let n = usize::from(*file.get(n_at).ok_or("truncated ipma")?);
        let end = n_at + 1 + n * if wide { 2 } else { 1 };
        if end > ipma.end {
            return Err("truncated ipma entry".into());
        }
        if id == primary {
            entry = Some((n_at, end));
            break;
        }
        at = end;
    }
    let (count_at, assoc_end) = entry.ok_or("primary item has no property associations")?;
    let mut assoc = Vec::new();
    for (i, p) in props.iter().enumerate() {
        let index = first_index + i;
        let flag = if p.essential { 0x80u16 } else { 0 };
        if wide {
            assoc.extend_from_slice(&((flag << 8) | index as u16).to_be_bytes());
        } else if index > 127 {
            return Err("too many properties for a narrow ipma".into());
        } else {
            assoc.push(flag as u8 | index as u8);
        }
    }
    let new_count = be(file, count_at, 1)? + props.len() as u64;
    if new_count > 255 {
        return Err("too many associations".into());
    }
    let props_bytes: Vec<u8> = props.iter().flat_map(|p| p.data.iter().copied()).collect();
    let delta = (props_bytes.len() + assoc.len()) as u64;

    let mut ev = vec![
        Event::Insert(ipco.end, props_bytes.clone()),
        Event::Insert(assoc_end, assoc.clone()),
        Event::Patch(count_at, 1, new_count),
        Event::Patch(ipco.start, 4, (ipco.end - ipco.start + props_bytes.len()) as u64),
        Event::Patch(ipma.start, 4, (ipma.end - ipma.start + assoc.len()) as u64),
        Event::Patch(iprp.start, 4, (iprp.end - iprp.start) as u64 + delta),
        Event::Patch(meta.start, 4, (meta.end - meta.start) as u64 + delta),
    ];

    // iloc: offsets that point behind the meta box move by `delta`.
    let iloc = find(&children, b"iloc")?;
    let v = be(file, iloc.body, 1)?;
    let sizes = be(file, iloc.body + 4, 2)? as usize;
    let (offset_size, length_size, base_size, index_size) = (sizes >> 12, (sizes >> 8) & 15, (sizes >> 4) & 15, if v == 1 || v == 2 { sizes & 15 } else { 0 });
    if v > 2 {
        return Err("unknown iloc version".into());
    }
    let (n_items, mut p) = if v < 2 { (be(file, iloc.body + 6, 2)? as usize, iloc.body + 8) } else { (be(file, iloc.body + 6, 4)? as usize, iloc.body + 10) };
    let moves = |v: u64| v >= meta.end as u64;
    for _ in 0..n_items {
        p += if v < 2 { 2 } else { 4 };
        let method = if v == 1 || v == 2 { (be(file, p, 2)? & 15) as u8 } else { 0 };
        if v == 1 || v == 2 {
            p += 2;
        }
        p += 2; // data reference index
        let base_at = p;
        let base = be(file, p, base_size)?;
        p += base_size;
        let extents = be(file, p, 2)? as usize;
        p += 2;
        for _ in 0..extents {
            p += index_size;
            let off_at = p;
            let off = be(file, p, offset_size)?;
            p += offset_size + length_size;
            if method == 0 && base_size == 0 && offset_size > 0 && moves(off) {
                ev.push(Event::Patch(off_at, offset_size, off + delta));
            }
        }
        if method == 0 && base_size > 0 && moves(base) {
            ev.push(Event::Patch(base_at, base_size, base + delta));
        }
    }
    if p > iloc.end {
        return Err("iloc overruns its box".into());
    }

    ev.sort_by_key(Event::pos);
    let mut out = Vec::with_capacity(file.len() + delta as usize);
    let mut cursor = 0usize;
    for e in &ev {
        let pos = e.pos();
        if pos < cursor {
            return Err("overlapping edits".into());
        }
        out.extend_from_slice(&file[cursor..pos]);
        cursor = pos;
        match e {
            Event::Insert(_, bytes) => out.extend_from_slice(bytes),
            Event::Patch(_, width, value) => {
                if *width < 8 && *value >> (8 * width) != 0 {
                    return Err("value does not fit its field".into());
                }
                out.extend_from_slice(&value.to_be_bytes()[8 - width..]);
                cursor = pos + width;
            }
        }
    }
    out.extend_from_slice(&file[cursor..]);
    Ok(out)
}

// ---------------------------------------------------------------- HEVC VUI colour description

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn u(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.d.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(if zeros > 0 { self.u(zeros)? } else { 0 })) as u32)
    }
}

/// Removes emulation-prevention bytes; `map[i]` is the index in `escaped` of raw byte `i`.
fn unescape(escaped: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let (mut raw, mut map, mut zeros) = (Vec::new(), Vec::new(), 0);
    for (i, b) in escaped.iter().enumerate() {
        if zeros >= 2 && *b == 3 {
            zeros = 0;
            continue;
        }
        raw.push(*b);
        map.push(i);
        zeros = if *b == 0 { zeros + 1 } else { 0 };
    }
    (raw, map)
}

fn escape(raw: &[u8]) -> Vec<u8> {
    let (mut out, mut zeros) = (Vec::with_capacity(raw.len() + 4), 0);
    for b in raw {
        if zeros >= 2 && *b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(*b);
        zeros = if *b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Bit position (in the unescaped SPS payload after the 2-byte NAL header) of the VUI's
/// `video_full_range_flag`, followed by colour_description_present_flag and the three CICP bytes.
/// `None` for SPS layouts this reader does not follow (sub-layers, scaling lists, PCM, reference
/// picture sets, no VUI colour description).
fn vui_colour_bit(raw: &[u8]) -> Option<usize> {
    let mut b = Bits { d: raw, pos: 0 };
    b.u(4)?;
    if b.u(3)? != 0 {
        return None; // sub-layers: profile_tier_level carries extra fields
    }
    b.u(1)?;
    b.u(8 + 32 + 48 + 8)?; // profile_tier_level(1, 0): general profile (88 bits) and level
    b.ue()?; // sps id
    let chroma = b.ue()?;
    if chroma == 3 {
        b.u(1)?;
    }
    b.ue()?;
    b.ue()?; // size
    if b.u(1)? == 1 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    b.ue()?;
    b.ue()?; // bit depths
    b.ue()?; // poc lsb
    b.u(1)?; // sub_layer_ordering_info_present_flag (one sub-layer: one set of values either way)
    for _ in 0..3 {
        b.ue()?;
    }
    for _ in 0..6 {
        b.ue()?; // coding / transform block sizes and hierarchy depths
    }
    if b.u(1)? == 1 {
        return None; // scaling lists
    }
    b.u(2)?; // amp, sao
    if b.u(1)? == 1 {
        return None; // pcm
    }
    if b.ue()? != 0 {
        return None; // short-term reference picture sets
    }
    if b.u(1)? == 1 && b.ue()? != 0 {
        return None; // long-term reference pictures in the SPS
    }
    b.u(2)?; // temporal mvp, strong intra smoothing
    if b.u(1)? == 0 {
        return None; // no VUI
    }
    if b.u(1)? == 1 && b.u(8)? == 255 {
        b.u(32)?;
    }
    if b.u(1)? == 1 {
        b.u(1)?;
    }
    if b.u(1)? == 0 {
        return None; // no video signal type
    }
    b.u(3)?; // video_format
    let at = b.pos; // video_full_range_flag
    b.u(1)?;
    (b.u(1)? == 1).then_some(at)
}

fn hvcc_sps(file: &[u8]) -> Result<(usize, usize), String> {
    let top = boxes(file, 0, file.len())?;
    let meta = find(&top, b"meta")?;
    let kids = boxes(file, meta.body + 4, meta.end)?;
    let iprp = find(&kids, b"iprp")?;
    let inner = boxes(file, iprp.body, iprp.end)?;
    let ipco = find(&inner, b"ipco")?;
    let props = boxes(file, ipco.body, ipco.end)?;
    let hvcc = find(&props, b"hvcC")?;
    let arrays = *file.get(hvcc.body + 22).ok_or("short hvcC")? as usize;
    let mut p = hvcc.body + 23;
    for _ in 0..arrays {
        let kind = file.get(p).ok_or("short hvcC")? & 0x3f;
        let n = be(file, p + 1, 2)? as usize;
        p += 3;
        for _ in 0..n {
            let len = be(file, p, 2)? as usize;
            if kind == 33 {
                if p + 2 + len > hvcc.end {
                    return Err("SPS overruns hvcC".into());
                }
                return Ok((p + 2, len));
            }
            p += 2 + len;
        }
    }
    Err("no SPS in hvcC".into())
}

/// The colour description the file's SPS declares: `(primaries, transfer, matrix, full_range)`.
#[cfg(test)]
pub(crate) fn vui_colour(file: &[u8]) -> Option<(u8, u8, u8, bool)> {
    let (at, len) = hvcc_sps(file).ok()?;
    let (raw, _) = unescape(file.get(at + 2..at + len)?);
    let bit = vui_colour_bit(&raw)?;
    let mut b = Bits { d: &raw, pos: bit };
    let full = b.u(1)? == 1;
    b.u(1)?;
    Some((b.u(8)? as u8, b.u(8)? as u8, b.u(8)? as u8, full))
}

/// Writes the colour description into the SPS of the file in place (same size: the VUI of the
/// Windows encoder already carries the colour fields, with an unspecified matrix). Decoders such
/// as ffmpeg take the matrix from there and ignore the container's `colr` box.
pub(crate) fn patch_vui_colour(file: &mut [u8], primaries: u8, transfer: u8, matrix: u8, full_range: bool) -> Result<(), String> {
    let (at, len) = hvcc_sps(file)?;
    let escaped = file.get(at + 2..at + len).ok_or("short SPS")?.to_vec();
    let (mut raw, _) = unescape(&escaped);
    let bit = vui_colour_bit(&raw).ok_or("the SPS has no colour description this editor can follow")?;
    let mut put = |pos: usize, n: usize, value: u32| {
        for i in 0..n {
            let at = pos + i;
            let v = (value >> (n - 1 - i)) & 1;
            let mask = 0x80u8 >> (at % 8);
            if let Some(byte) = raw.get_mut(at / 8) {
                *byte = (*byte & !mask) | if v == 1 { mask } else { 0 };
            }
        }
    };
    put(bit, 1, u32::from(full_range));
    put(bit + 2, 8, u32::from(primaries));
    put(bit + 10, 8, u32::from(transfer));
    put(bit + 18, 8, u32::from(matrix));
    let again = escape(&raw);
    if again.len() != escaped.len() {
        return Err("the new colour fields need different emulation prevention".into());
    }
    file[at + 2..at + len].copy_from_slice(&again);
    Ok(())
}

/// For a tile-grid file encoded on a padded canvas: states the real picture size in the grid
/// descriptor (`idat`) and in the `ispe` property of the grid item, in place. The tiles still
/// cover the padded area, which the HEIF specification allows (the grid output may be smaller than
/// the tile area), so every reader shows the real size, including those that ignore `clap`.
/// `Ok(false)`: the file is not a tile grid (use `clap` instead).
pub(crate) fn crop_grid_to(file: &mut [u8], padded: (u32, u32), real: (u32, u32)) -> Result<bool, String> {
    let top = boxes(file, 0, file.len())?;
    let meta = find(&top, b"meta")?;
    let kids = boxes(file, meta.body + 4, meta.end)?;
    let Ok(idat) = find(&kids, b"idat") else { return Ok(false) };
    let iprp = find(&kids, b"iprp")?;
    let inner = boxes(file, iprp.body, iprp.end)?;
    let ipco = find(&inner, b"ipco")?;
    let (d, flags) = (idat.body, *file.get(idat.body + 1).ok_or("short idat")?);
    let wide = flags & 1 == 1;
    let (w_at, h_at, n) = if wide { (d + 4, d + 8, 4) } else { (d + 4, d + 6, 2) };
    if be(file, w_at, n)? != u64::from(padded.0) || be(file, h_at, n)? != u64::from(padded.1) {
        return Ok(false);
    }
    if !wide && (real.0 > 65535 || real.1 > 65535) {
        return Err("grid size does not fit".into());
    }
    let ispes: Vec<usize> = boxes(file, ipco.body, ipco.end)?
        .iter()
        .filter(|b| &b.kind == b"ispe" && b.end - b.body == 12 && be(file, b.body + 4, 4).ok() == Some(u64::from(padded.0)) && be(file, b.body + 8, 4).ok() == Some(u64::from(padded.1)))
        .map(|b| b.body + 4)
        .collect();
    if ispes.is_empty() {
        return Err("no ispe for the grid item".into());
    }
    let put = |f: &mut [u8], at: usize, n: usize, v: u32| f[at..at + n].copy_from_slice(&v.to_be_bytes()[4 - n..]);
    put(file, w_at, n, real.0);
    put(file, h_at, n, real.1);
    for at in ispes {
        put(file, at, 4, real.0);
        put(file, at + 4, 4, real.1);
    }
    Ok(true)
}

/// The `colr` payload types stored for the primary item, for tests: `(nclx bytes, has prof)`.
#[cfg(test)]
pub(crate) fn colr_kinds(file: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(rel) = file[at..].windows(4).position(|w| w == b"colr") {
        let p = at + rel;
        if p + 8 <= file.len() {
            out.push([file[p + 4], file[p + 5], file[p + 6], file[p + 7]]);
        }
        at = p + 4;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    /// ftyp, meta(hdlr, pitm 1, iloc v0 -> mdat, iprp(ipco(ispe, hvcC), ipma)), mdat "PAYLOAD!".
    fn sample() -> (Vec<u8>, usize) {
        let ftyp = bx(b"ftyp", b"heic\0\0\0\0mif1");
        let hdlr = bx(b"hdlr", &[0u8; 25]);
        let pitm = bx(b"pitm", &[0, 0, 0, 0, 0, 1]);
        let ispe = bx(b"ispe", &[0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0, 16]);
        let hvcc = bx(b"hvcC", &[1, 2, 3]);
        let ipco = bx(b"ipco", &[ispe, hvcc].concat());
        let ipma = bx(b"ipma", &[0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 2, 1, 0x82]);
        let iprp = bx(b"iprp", &[ipco, ipma].concat());
        // iloc v0: offset_size 4, length_size 4, base_size 0; 1 item, 1 extent.
        let iloc_len = 4 + 2 + 2 + (2 + 2 + 2 + 4 + 4);
        let mut iloc_body = vec![0, 0, 0, 0, 0x44, 0x00, 0, 1, 0, 1, 0, 0, 0, 1];
        let placeholder = iloc_body.len();
        iloc_body.extend_from_slice(&[0; 8]);
        assert_eq!(iloc_body.len(), iloc_len);
        let mut meta_body = vec![0u8; 4];
        meta_body.extend(hdlr);
        meta_body.extend(pitm);
        let iloc_pos_in_meta = meta_body.len() + 8 + placeholder;
        meta_body.extend(bx(b"iloc", &iloc_body));
        meta_body.extend(iprp);
        let meta = bx(b"meta", &meta_body);
        let mdat_start = ftyp.len() + meta.len() + 8;
        let mut file = ftyp.clone();
        file.extend(meta);
        let patch = ftyp.len() + 8 + iloc_pos_in_meta;
        file[patch..patch + 4].copy_from_slice(&(mdat_start as u32).to_be_bytes());
        file[patch + 4..patch + 8].copy_from_slice(&8u32.to_be_bytes());
        file.extend(bx(b"mdat", b"PAYLOAD!"));
        (file, mdat_start)
    }

    fn payload_at(file: &[u8]) -> usize {
        file.windows(8).position(|w| w == b"PAYLOAD!").unwrap()
    }

    #[test]
    fn properties_are_added_associated_and_offsets_follow() {
        let (file, mdat_start) = sample();
        assert_eq!(payload_at(&file), mdat_start);
        let props = [NewProp { data: nclx_box(1, 13, 1, true), essential: false }, NewProp { data: clap_box(15, 14, (16, 16)), essential: true }];
        let out = add_primary_properties(&file, &props).unwrap();
        assert_eq!(out.len(), file.len() + 19 + 40 + 2);
        // The box tree still parses with consistent sizes.
        let top = boxes(&out, 0, out.len()).unwrap();
        let meta = find(&top, b"meta").unwrap();
        let kids = boxes(&out, meta.body + 4, meta.end).unwrap();
        let iprp = find(&kids, b"iprp").unwrap();
        let inner = boxes(&out, iprp.body, iprp.end).unwrap();
        let ipco = find(&inner, b"ipco").unwrap();
        assert_eq!(boxes(&out, ipco.body, ipco.end).unwrap().len(), 4);
        // ipma: three associations now, the new ones are properties 3 and 4, clap essential.
        let ipma = find(&inner, b"ipma").unwrap();
        assert_eq!(&out[ipma.body + 8..ipma.end], &[0, 1, 4, 1, 0x82, 3, 0x84]);
        // iloc points at the payload again.
        let iloc = find(&kids, b"iloc").unwrap();
        let off = u32::from_be_bytes(out[iloc.body + 14..iloc.body + 18].try_into().unwrap()) as usize;
        assert_eq!(off, payload_at(&out));
        assert_eq!(colr_kinds(&out), vec![*b"nclx"]);
    }

    #[test]
    fn broken_files_are_errors() {
        let (file, _) = sample();
        let prop = [NewProp { data: nclx_box(1, 13, 1, true), essential: false }];
        assert!(add_primary_properties(&[], &prop).is_err());
        assert!(add_primary_properties(&file[..file.len() - 30], &prop).is_err());
        let mut no_meta = file.clone();
        no_meta[24..28].copy_from_slice(b"free");
        assert!(add_primary_properties(&no_meta, &prop).is_err());
        for cut in (0..file.len()).step_by(7) {
            let _ = add_primary_properties(&file[..cut], &prop);
        }
    }
}
