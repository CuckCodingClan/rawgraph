use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rawgraph_core::{CaptureInfo, ImageId};

use super::*;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rawgraph-project-extra-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.path(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, content).unwrap();
        p
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn supported(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("raw"))
}

fn no_capture(_: &Path) -> Option<CaptureInfo> {
    None
}

fn project_with(t: &Tmp, files: &[(&str, &str)]) -> Project {
    for (n, c) in files {
        t.write(n, c);
    }
    let mut p = Project::new();
    let r = p.scan_folder(&t.0, true, &supported, &no_capture);
    assert_eq!(r.added.len(), files.len());
    p
}

fn id_of(p: &Project, name: &str) -> ImageId {
    p.images().iter().find(|e| e.file_name() == name).unwrap().id
}

// ---- XMP parsing ----

#[test]
fn xmp_attribute_form() {
    let x = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
        <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4" xmp:Label="Red"/>
        </rdf:RDF></x:xmpmeta>"#;
    let c = parse_xmp_culling(x).unwrap();
    assert_eq!(c, XmpCulling { rating: Some(4), label: Some("Red".into()), ..Default::default() });
}

#[test]
fn xmp_element_form() {
    let x = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
        <rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/"><xmp:Rating>3</xmp:Rating>
        <xmp:Label> Blau </xmp:Label></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    let c = parse_xmp_culling(x).unwrap();
    assert_eq!(c, XmpCulling { rating: Some(3), label: Some("Blau".into()), ..Default::default() });
}

#[test]
fn xmp_prefix_variation() {
    let x = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description
        xmlns:a="http://ns.adobe.com/xap/1.0/" a:Rating="2" a:Label="Green"/></rdf:RDF>"#;
    assert_eq!(parse_xmp_culling(x).unwrap(), XmpCulling { rating: Some(2), label: Some("Green".into()), ..Default::default() });
    // A different namespace with the same local name is not the XMP rating.
    let y = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description
        xmlns:other="http://example.com/ns/" other:Rating="5"/></rdf:RDF>"#;
    assert_eq!(parse_xmp_culling(y).unwrap(), XmpCulling::default());
}

#[test]
fn xmp_lightroom_sidecar() {
    let x = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00\">\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\"\n\
    xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n\
    xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"\n\
    xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"\n\
    xmlns:xmpMM=\"http://ns.adobe.com/xap/1.0/mm/\"\n\
    xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n\
    xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\"\n\
   xmp:Rating=\"5\"\n\
   xmp:CreatorTool=\"Adobe Lightroom Classic 13.0 (Windows)\"\n\
   xmp:ModifyDate=\"2024-05-01T10:11:12\"\n\
   xmp:Label=\"Lila\"\n\
   tiff:Make=\"SONY\"\n\
   tiff:Model=\"ILCE-7M4\"\n\
   crs:Version=\"16.0\" crs:Exposure2012=\"+0.35\" crs:Temperature=\"5200\">\n\
   <dc:subject><rdf:Bag><rdf:li>wedding</rdf:li></rdf:Bag></dc:subject>\n\
   <xmpMM:History><rdf:Seq><rdf:li stEvt:action=\"saved\" xmlns:stEvt=\"http://ns.adobe.com/xap/1.0/sType/ResourceEvent#\"/></rdf:Seq></xmpMM:History>\n\
  </rdf:Description>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n<?xpacket end=\"w\"?>";
    assert_eq!(parse_xmp_culling(x).unwrap(), XmpCulling { rating: Some(5), label: Some("Lila".into()), keywords: vec!["wedding".into()] });
}

#[test]
fn xmp_rejected_missing_and_garbage() {
    let neg = r#"<rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="-1"/>"#;
    assert_eq!(parse_xmp_culling(neg).unwrap().rating, Some(-1));
    let none = r#"<rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:CreatorTool="x"/>"#;
    assert_eq!(parse_xmp_culling(none).unwrap(), XmpCulling::default());
    let weird = r#"<d xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="99" xmp:Label="  "/>"#;
    assert_eq!(parse_xmp_culling(weird).unwrap(), XmpCulling::default());
    for g in ["", "hello", "<<<>>>", "<a><b></a>", "\u{0}\u{1}\u{2}", "<a xmp:Rating=3>"] {
        let _ = parse_xmp_culling(g); // must not panic
    }
    // Large junk is rejected or processed quickly.
    let start = std::time::Instant::now();
    let junk = "x".repeat(5 * 1024 * 1024);
    assert!(parse_xmp_culling(&junk).is_err());
    let junk2 = "<a ".repeat(1_000_000);
    let _ = parse_xmp_culling(&junk2);
    let junk3 = "&;<!--".repeat(500_000);
    let _ = parse_xmp_culling(&junk3);
    assert!(start.elapsed() < Duration::from_secs(20));
}

#[test]
fn xmp_label_names_and_apply() {
    let t = Tmp::new();
    let mut p = project_with(&t, &[("a.raw", "a")]);
    let id = id_of(&p, "a.raw");
    let cases = [
        ("Red", ColorLabel::Red),
        ("yellow", ColorLabel::Yellow),
        ("GREEN", ColorLabel::Green),
        ("Blue", ColorLabel::Blue),
        ("Purple", ColorLabel::Purple),
        ("Rot", ColorLabel::Red),
        ("Gelb", ColorLabel::Yellow),
        ("Gr\u{fc}n", ColorLabel::Green),
        ("Blau", ColorLabel::Blue),
        ("Lila", ColorLabel::Purple),
    ];
    for (name, want) in cases {
        p.image_mut(id).unwrap().color_label = None;
        let x = XmpCulling { rating: None, label: Some(name.into()), ..Default::default() };
        assert!(p.apply_xmp_culling(id, &x), "{name}");
        assert_eq!(p.image(id).unwrap().color_label, Some(want));
        assert!(!p.apply_xmp_culling(id, &x), "second apply changes nothing");
    }
    let unknown = XmpCulling { rating: Some(0), label: Some("Pink".into()), ..Default::default() };
    assert!(!p.apply_xmp_culling(id, &unknown));
    assert!(p.apply_xmp_culling(id, &XmpCulling { rating: Some(4), label: None, ..Default::default() }));
    assert_eq!(p.image(id).unwrap().rating, 4);
    assert!(p.apply_xmp_culling(id, &XmpCulling { rating: Some(-1), label: None, ..Default::default() }));
    let e = p.image(id).unwrap();
    assert_eq!((e.flag, e.rating), (Flag::Reject, 4));
    assert!(!p.apply_xmp_culling(ImageId(999), &XmpCulling { rating: Some(3), label: None, ..Default::default() }));
}

fn sidecar(rating: &str, label: &str) -> String {
    format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="{rating}" xmp:Label="{label}"/></rdf:RDF></x:xmpmeta>"#
    )
}

#[test]
fn import_sidecars_only_unrated_and_broken() {
    let t = Tmp::new();
    let mut p = project_with(&t, &[("a.raw", "a"), ("b.raw", "b"), ("c.raw", "c"), ("d.raw", "d"), ("e.raw", "e")]);
    t.write("a.xmp", &sidecar("3", "Red"));
    t.write("b.XMP", &sidecar("5", "Blue"));
    t.write("c.xmp", "this is <<< not xml");
    t.write("d.xmp", &"x".repeat(5 * 1024 * 1024));
    // e has no sidecar.
    let (a, b, c, d, e) =
        (id_of(&p, "a.raw"), id_of(&p, "b.raw"), id_of(&p, "c.raw"), id_of(&p, "d.raw"), id_of(&p, "e.raw"));
    // The user already rated b in RawGraph.
    p.image_mut(b).unwrap().rating = 2;
    let changed = p.import_xmp_sidecars(true);
    assert_eq!(changed, vec![a]);
    assert_eq!(p.image(a).unwrap().rating, 3);
    assert_eq!(p.image(a).unwrap().color_label, Some(ColorLabel::Red));
    assert_eq!(p.image(b).unwrap().rating, 2);
    for id in [c, d, e] {
        assert_eq!(p.image(id).unwrap().rating, 0);
    }
    let changed = p.import_xmp_sidecars(false);
    assert_eq!(changed, vec![b]);
    assert_eq!(p.image(b).unwrap().rating, 5);
    assert_eq!(p.image(b).unwrap().color_label, Some(ColorLabel::Blue));
    // Missing entries are skipped.
    p.image_mut(a).unwrap().missing = true;
    p.image_mut(a).unwrap().rating = 1;
    assert!(p.import_xmp_sidecars(false).is_empty());
    assert_eq!(p.image(a).unwrap().rating, 1);
}

#[test]
fn sidecar_path_case_variants() {
    let t = Tmp::new();
    let img = t.write("IMG_1.CR2", "x");
    assert_eq!(xmp_sidecar_path(&img), None);
    t.write("IMG_1.xmp", "<a/>");
    let p = xmp_sidecar_path(&img).unwrap();
    assert!(p.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("IMG_1.xmp"));
    let img2 = t.write("IMG_2.RAW", "y");
    t.write("IMG_2.XMP", "<a/>");
    let p2 = xmp_sidecar_path(&img2).unwrap();
    assert!(p2.file_name().unwrap().to_string_lossy().eq_ignore_ascii_case("IMG_2.xmp"));
    let img3 = t.write("IMG_3.raw", "z");
    t.write("IMG_3.Xmp", "<a/>");
    assert!(xmp_sidecar_path(&img3).is_some());
    // A different stem does not match.
    let img4 = t.write("IMG_4.raw", "w");
    t.write("IMG_44.xmp", "<a/>");
    assert_eq!(xmp_sidecar_path(&img4), None);
}

// ---- refresh_capture ----

#[test]
fn refresh_capture_counts_changes_only() {
    let t = Tmp::new();
    let mut p = project_with(&t, &[("a.raw", "a"), ("b.raw", "b"), ("c.raw", "c"), ("d.raw", "d")]);
    let (a, b, c, d) = (id_of(&p, "a.raw"), id_of(&p, "b.raw"), id_of(&p, "c.raw"), id_of(&p, "d.raw"));
    p.image_mut(d).unwrap().missing = true;
    p.image_mut(b).unwrap().capture = CaptureInfo { iso: Some(200.0), ..Default::default() };
    p.image_mut(c).unwrap().capture = CaptureInfo { iso: Some(400.0), ..Default::default() };
    let reader = |path: &Path| -> Option<CaptureInfo> {
        match path.file_name()?.to_str()? {
            "a.raw" => Some(CaptureInfo { iso: Some(100.0), ..Default::default() }),
            "b.raw" => Some(CaptureInfo { iso: Some(200.0), ..Default::default() }), // same as stored
            "d.raw" => Some(CaptureInfo { iso: Some(800.0), ..Default::default() }),
            _ => None, // c: reader fails, leave unchanged
        }
    };
    let n = p.refresh_capture(&[a, b, c, d, ImageId(777), a], &reader);
    assert_eq!(n, 1);
    assert_eq!(p.image(a).unwrap().capture.iso, Some(100.0));
    assert_eq!(p.image(b).unwrap().capture.iso, Some(200.0));
    assert_eq!(p.image(c).unwrap().capture.iso, Some(400.0));
    assert_eq!(p.image(d).unwrap().capture.iso, None);
}

// ---- relink ----

#[test]
fn relink_success_mismatch_force_used_and_missing() {
    let t = Tmp::new();
    let mut p = project_with(&t, &[("a.raw", "aaaa"), ("b.raw", "bbbb")]);
    let (a, b) = (id_of(&p, "a.raw"), id_of(&p, "b.raw"));
    {
        let e = p.image_mut(a).unwrap();
        e.rating = 4;
        e.flag = Flag::Pick;
        e.color_label = Some(ColorLabel::Green);
        e.capture.iso = Some(640.0);
    }
    let hash_a = p.image(a).unwrap().content_hash.clone();
    fs::remove_file(t.path("a.raw")).unwrap();
    assert_eq!(p.refresh_missing(), 1);
    assert!(p.image(a).unwrap().missing);

    // Same content elsewhere: relinks without force.
    let moved = t.write("sub/a_moved.raw", "aaaa");
    p.relink(a, &moved, false).unwrap();
    let e = p.image(a).unwrap();
    assert!(!e.missing);
    assert_eq!(e.path, std::path::absolute(&moved).unwrap());
    assert_eq!((e.rating, e.flag, e.color_label, e.capture.iso), (4, Flag::Pick, Some(ColorLabel::Green), Some(640.0)));
    assert_eq!(e.content_hash, hash_a);

    // Mismatch without force, accepted with force.
    let other = t.write("other.raw", "different content");
    let err = p.relink(a, &other, false).unwrap_err();
    assert!(matches!(err, ProjectError::Mismatch(_)), "{err}");
    assert_eq!(p.image(a).unwrap().path, std::path::absolute(&moved).unwrap());
    p.image_mut(a).unwrap().missing = true;
    p.relink(a, &other, true).unwrap();
    let e = p.image(a).unwrap();
    assert!(!e.missing);
    assert_eq!(e.file_size, "different content".len() as u64);
    assert_ne!(e.content_hash, hash_a);
    assert_eq!(e.id, a);
    assert_eq!(e.rating, 4);

    // Path already used by another entry.
    let err = p.relink(b, &other, true).unwrap_err();
    assert!(matches!(err, ProjectError::Mismatch(m) if m.contains("already used")));
    // Nonexistent file, unknown id.
    assert!(p.relink(b, &t.path("nope.raw"), true).is_err());
    assert!(p.relink(ImageId(555), &other, true).is_err());
    // Relinking to its own path is fine.
    p.relink(b, &t.path("b.raw"), false).unwrap();
}

#[test]
fn relink_folder_partial() {
    let t = Tmp::new();
    let mut p = project_with(&t, &[("old/a.raw", "a"), ("old/s/b.raw", "b"), ("old/c.raw", "c"), ("keep/d.raw", "d")]);
    t.write("new/a.raw", "a");
    t.write("new/s/b.raw", "b");
    // new/c.raw does not exist.
    fs::remove_dir_all(t.path("old")).unwrap();
    p.refresh_missing();
    assert_eq!(p.images().iter().filter(|e| e.missing).count(), 3);
    let n = p.relink_folder(&t.path("old"), &t.path("new"));
    assert_eq!(n, 2);
    let a = p.image(id_of(&p, "a.raw")).unwrap();
    assert!(!a.missing && a.path == std::path::absolute(t.path("new/a.raw")).unwrap());
    let b = p.image(id_of(&p, "b.raw")).unwrap();
    assert!(!b.missing && b.path == std::path::absolute(t.path("new").join("s").join("b.raw")).unwrap());
    assert!(p.image(id_of(&p, "c.raw")).unwrap().missing);
    assert!(!p.image(id_of(&p, "d.raw")).unwrap().missing);
    assert_eq!(p.relink_folder(&t.path("nothing"), &t.path("new")), 0);
}

// ---- recovery ----

#[test]
fn recovery_save_list_load_discard() {
    let t = Tmp::new();
    let mut p = project_with(&t, &[("a.raw", "a"), ("b.raw", "b")]);
    p.image_mut(id_of(&p, "a.raw")).unwrap().rating = 5;
    let dir = t.path("rec");
    let proj_path = t.path("job.rawgraph");

    let f1 = p.save_recovery(&dir, Some(&proj_path), "s1").unwrap();
    p.image_mut(id_of(&p, "b.raw")).unwrap().rating = 2;
    let f2 = p.save_recovery(&dir, Some(&proj_path), "other-session").unwrap();
    assert_eq!(f1, f2);
    assert_eq!(list_recoveries(&dir).len(), 1);

    std::thread::sleep(Duration::from_millis(5));
    let q = Project::new();
    let f3 = q.save_recovery(&dir, None, "session-x").unwrap();
    assert_ne!(f3, f1);
    let f3b = q.save_recovery(&dir, None, "session-x").unwrap();
    assert_eq!(f3, f3b);
    let f4 = q.save_recovery(&dir, None, "session-y").unwrap();
    assert_ne!(f4, f3);
    let list = list_recoveries(&dir);
    assert_eq!(list.len(), 3);
    assert!(list.windows(2).all(|w| w[0].saved_unix_ms >= w[1].saved_unix_ms));
    let with_path = list.iter().find(|i| i.file == f1).unwrap();
    assert_eq!(with_path.project_path, Some(std::path::absolute(&proj_path).unwrap()));
    assert_eq!(with_path.image_count, 2);
    assert_eq!(list.iter().filter(|i| i.project_path.is_none()).count(), 2);

    let (loaded, info) = load_recovery(&f1).unwrap();
    assert_eq!(loaded, p);
    assert_eq!(info.image_count, 2);
    assert_eq!(Project::load(&f1).unwrap(), p);

    // Stray and broken files are skipped.
    fs::write(dir.join("recovery-garbage.rgrecovery"), "nope").unwrap();
    fs::write(dir.join("notes.txt"), "x").unwrap();
    assert_eq!(list_recoveries(&dir).len(), 3);

    discard_recovery(&f3).unwrap();
    assert!(!f3.exists());
    assert_eq!(list_recoveries(&dir).len(), 2);
}

#[test]
fn discard_refuses_foreign_files() {
    let t = Tmp::new();
    let p = Project::new();
    // A project file named like a recovery file but outside a recovery dir.
    let outside = t.path("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    let fake = outside.join("recovery-abc.rgrecovery");
    p.save(&fake).unwrap();
    assert!(discard_recovery(&fake).is_err());
    assert!(fake.exists());
    // Inside a recovery dir but without the recovery marker in the database.
    let dir = t.path("rec");
    p.save_recovery(&dir, None, "k").unwrap();
    let plain = dir.join("recovery-plain.rgrecovery");
    p.save(&plain).unwrap();
    assert!(discard_recovery(&plain).is_err());
    assert!(plain.exists());
    // A normal project file inside the recovery dir, and a missing file.
    let proj = dir.join("job.rawgraph");
    p.save(&proj).unwrap();
    assert!(discard_recovery(&proj).is_err());
    assert!(proj.exists());
    assert!(discard_recovery(&dir.join("recovery-none.rgrecovery")).is_err());
    // A user project elsewhere.
    let user_proj = t.path("userproj.rawgraph");
    p.save(&user_proj).unwrap();
    assert!(discard_recovery(&user_proj).is_err());
    assert!(user_proj.exists());
}

// ---- thumbnail cache ----

#[test]
fn thumb_cache_removes_one_size_or_all_sizes_of_a_content() {
    let t = Tmp::new();
    let c = ThumbCache::new(&t.path("thumbs"), 10_000).unwrap();
    c.put("abc123", 256, b"hello").unwrap();
    c.put("abc123", 512, b"world!").unwrap();
    c.put("abc124", 256, b"other").unwrap();
    assert!(c.remove("abc123", Some(256)));
    assert!(!c.remove("abc123", Some(256)));
    assert_eq!(c.get("abc123", 256), None);
    assert!(c.get("abc123", 512).is_some());
    assert!(c.remove("ABC123", None));
    assert_eq!(c.get("abc123", 512), None);
    assert_eq!(c.get("abc124", 256).unwrap(), b"other");
    assert_eq!(c.total_bytes(), 5);
    assert!(!c.remove("../evil", None) && !c.remove("", Some(1)));
}

#[test]
fn thumb_cache_roundtrip_and_validation() {
    let t = Tmp::new();
    let c = ThumbCache::new(&t.path("thumbs"), 10_000).unwrap();
    assert_eq!(c.get("abc123", 256), None);
    c.put("abc123", 256, b"hello").unwrap();
    c.put("abc123", 512, b"world!").unwrap();
    assert_eq!(c.get("abc123", 256).unwrap(), b"hello");
    assert_eq!(c.get("ABC123", 512).unwrap(), b"world!");
    assert_eq!(c.total_bytes(), 11);
    c.put("abc123", 256, b"hi").unwrap();
    assert_eq!(c.get("abc123", 256).unwrap(), b"hi");
    let long = "a".repeat(129);
    for bad in ["", "../evil", "zz", "ab/cd", "ab\\cd", "abc.thumb", long.as_str()] {
        assert!(c.put(bad, 1, b"x").is_err(), "{bad}");
        assert_eq!(c.get(bad, 1), None);
    }
    assert!(!t.0.join("evil").exists());
    assert_eq!(c.total_bytes(), 8);
}

#[test]
fn thumb_cache_evicts_least_recently_used() {
    let t = Tmp::new();
    let dir = t.path("thumbs");
    let c = ThumbCache::new(&dir, 1000).unwrap();
    let blob = vec![7u8; 100];
    for h in ["a1", "b2", "c3"] {
        c.put(h, 128, &blob).unwrap();
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(c.get("a1", 128).is_some()); // refresh a1: b2 is now the oldest
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(c.evict(), 0);
    // Opening with a smaller budget trims oldest-first immediately.
    let small = ThumbCache::new(&dir, 200).unwrap();
    assert_eq!(small.evict(), 0);
    assert!(small.get("b2", 128).is_none());
    assert!(small.get("a1", 128).is_some());
    assert!(small.get("c3", 128).is_some());
    assert_eq!(small.total_bytes(), 200);
    // A put beyond the budget evicts automatically.
    std::thread::sleep(Duration::from_millis(40));
    small.put("d4", 128, &blob).unwrap();
    assert!(small.total_bytes() <= 200);
    assert!(small.get("d4", 128).is_some());
}

#[test]
fn thumb_cache_concurrent() {
    let t = Tmp::new();
    let c = ThumbCache::new(&t.path("thumbs"), 1_000_000).unwrap();
    std::thread::scope(|s| {
        for th in 0..8u32 {
            let c = &c;
            s.spawn(move || {
                for i in 0..40u32 {
                    let hash = format!("{:04x}", i % 10);
                    let bytes = vec![(i % 10) as u8; 1000];
                    c.put(&hash, 256, &bytes).unwrap();
                    if let Some(got) = c.get(&hash, 256) {
                        assert_eq!(got, bytes, "thread {th}");
                    }
                }
            });
        }
    });
    assert_eq!(c.total_bytes(), 10_000);
}
