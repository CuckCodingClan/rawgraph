//! Keywords in rendered exports: the XMP packet (`dc:subject`, `lr:hierarchicalSubject`) embedded in
//! JPEG, PNG, TIFF (8 and 16 bit) and WebP files, read back with `read_xmp_packet`.

use std::path::PathBuf;

use rawgraph_core::color::OutputSpace;
use rawgraph_io::*;

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("rawgraph-io-kw-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        Dir(p)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rgb8(w: u32, h: u32) -> Vec<u8> {
    (0..w * h).flat_map(|i| [(i * 7 % 256) as u8, (i * 3 % 256) as u8, (i % 256) as u8]).collect()
}

fn keywords() -> Vec<String> {
    vec!["Reise|Österreich|Wörthersee".into(), "Tom & <Jerry>".into(), "日本".into(), "Food".into()]
}

fn md(keywords: Vec<String>) -> ExportMetadata {
    ExportMetadata { make: Some("SONY".into()), keywords, ..Default::default() }
}

fn opts(keywords: Vec<String>) -> SaveOptions {
    SaveOptions { jpeg_quality: 90, overwrite: true, embed_profile: true, space: OutputSpace::Srgb, metadata: Some(md(keywords)) }
}

/// What the packet must say, whatever the container.
fn assert_keyword_packet(packet: &str) {
    assert!(packet.starts_with("<?xpacket"), "{packet}");
    assert!(packet.contains("<dc:subject>"), "{packet}");
    for leaf in ["Wörthersee", "Tom &amp; &lt;Jerry&gt;", "日本", "Food"] {
        assert!(packet.contains(&format!("<rdf:li>{leaf}</rdf:li>")), "{leaf} missing in {packet}");
    }
    assert!(packet.contains("<lr:hierarchicalSubject>"), "{packet}");
    assert!(packet.contains("<rdf:li>Reise|Österreich|Wörthersee</rdf:li>"), "{packet}");
}

#[test]
fn jpeg_png_and_tiff_carry_the_keywords_and_still_decode() {
    let d = Dir::new("8bit");
    let px = rgb8(16, 16);
    for name in ["k.jpg", "k.png", "k.tif"] {
        let p = d.file(name);
        save_srgb8_with(&p, 16, 16, &px, &opts(keywords())).expect(name);
        assert_keyword_packet(&read_xmp_packet(&p).unwrap_or_else(|| panic!("no packet in {name}")));
        assert!(image::open(&p).is_ok(), "{name} must still decode");
        // The EXIF block is still there next to the packet.
        assert_eq!(read_export_metadata(&p).make.as_deref(), Some("SONY"), "{name}");
    }
    // Without keywords there is no packet at all.
    for name in ["n.jpg", "n.png", "n.tif"] {
        let p = d.file(name);
        save_srgb8_with(&p, 16, 16, &px, &opts(Vec::new())).expect(name);
        assert_eq!(read_xmp_packet(&p), None, "{name}");
    }
    // JPEG: the Exif segment is still the first APP1 segment (right after JFIF or SOI).
    let jpg = std::fs::read(d.file("k.jpg")).expect("read");
    let exif_at = jpg.windows(6).position(|w| w == b"Exif\0\0").expect("exif");
    let xmp_at = jpg.windows(28).position(|w| w == b"http://ns.adobe.com/xap/1.0/").expect("xmp");
    assert!(exif_at < xmp_at, "Exif first");
}

#[test]
fn sixteen_bit_png_and_tiff_carry_the_keywords() {
    let d = Dir::new("16bit");
    let px: Vec<u16> = rgb8(8, 8).iter().map(|v| u16::from(*v) * 257).collect();
    for name in ["k16.png", "k16.tif"] {
        let p = d.file(name);
        save_srgb16_with(&p, 8, 8, &px, &opts(keywords())).expect(name);
        assert_keyword_packet(&read_xmp_packet(&p).unwrap_or_else(|| panic!("no packet in {name}")));
        assert!(image::open(&p).is_ok(), "{name}");
    }
}

#[test]
fn webp_lossy_and_lossless_carry_the_keywords_and_still_decode() {
    let d = Dir::new("webp");
    let (w, h) = (24u32, 16u32);
    let rgba: Vec<u8> = (0..w * h).flat_map(|i| [(i % 256) as u8, (i * 5 % 256) as u8, 40, 255]).collect();
    for (name, lossless) in [("k-lossless.webp", true), ("k-lossy.webp", false)] {
        let p = d.file(name);
        save_webp8(&p, w, h, &rgba, lossless, &opts(keywords())).expect(name);
        assert_keyword_packet(&read_xmp_packet(&p).unwrap_or_else(|| panic!("no packet in {name}")));
        let bytes = std::fs::read(&p).expect("read");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize, bytes.len() - 8, "RIFF size");
        let decoded = webp_rust::decode(&bytes).expect("still decodes");
        assert_eq!((decoded.width as u32, decoded.height as u32), (w, h));
        // VP8X header with the XMP flag.
        let at = bytes.windows(4).position(|x| x == b"VP8X").expect("VP8X");
        assert_ne!(bytes[at + 8] & 0x04, 0, "XMP flag");
        let plain = d.file(&format!("plain-{name}"));
        save_webp8(&plain, w, h, &rgba, lossless, &opts(Vec::new())).expect("plain");
        assert_eq!(read_xmp_packet(&plain), None);
    }
}

#[test]
fn a_huge_keyword_list_is_cut_to_fit_the_jpeg_segment() {
    let d = Dir::new("huge");
    let many: Vec<String> = (0..20_000).map(|i| format!("keyword number {i:05}")).collect();
    let p = d.file("big.jpg");
    save_srgb8_with(&p, 8, 8, &rgb8(8, 8), &opts(many)).expect("write");
    let packet = read_xmp_packet(&p).expect("a shortened packet");
    assert!(packet.len() < 65_000 && packet.contains("keyword number 00000"));
    assert!(image::open(&p).is_ok());
}

#[test]
fn the_keywords_switch_of_the_metadata_selection() {
    let full = ExportMetadata { keywords: keywords(), ..Default::default() };
    let on = rawgraph_core::MetaSelection::default();
    assert_eq!(full.select(&on).keywords, keywords());
    assert!(full.select(&rawgraph_core::MetaSelection { keywords: false, ..on }).keywords.is_empty());
    let d = Dir::new("switch");
    let p = d.file("s.jpg");
    save_srgb8_with(&p, 8, 8, &rgb8(8, 8), &opts(full.select(&rawgraph_core::MetaSelection { keywords: false, ..on }).keywords)).expect("write");
    assert_eq!(read_xmp_packet(&p), None);
}
