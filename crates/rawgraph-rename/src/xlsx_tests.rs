//! Reads a real (generated) .xlsx through calamine. The workbook is built here with a minimal zip
//! writer (stored entries), so no archive crate is needed for the tests.

use std::path::Path;

use crate::sheet::{CaptionSheet, SheetColumns};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn zip_stored(files: &[(&str, String)]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let u16le = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    let u32le = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    for (name, content) in files {
        let data = content.as_bytes();
        let crc = crc32(data);
        let offset = out.len() as u32;
        u32le(&mut out, 0x0403_4b50);
        for x in [20u16, 0, 0, 0, 33] {
            u16le(&mut out, x);
        }
        u32le(&mut out, crc);
        u32le(&mut out, data.len() as u32);
        u32le(&mut out, data.len() as u32);
        u16le(&mut out, name.len() as u16);
        u16le(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        u32le(&mut central, 0x0201_4b50);
        for x in [20u16, 20, 0, 0, 0, 33] {
            u16le(&mut central, x);
        }
        u32le(&mut central, crc);
        u32le(&mut central, data.len() as u32);
        u32le(&mut central, data.len() as u32);
        for x in [name.len() as u16, 0, 0, 0, 0] {
            u16le(&mut central, x);
        }
        u32le(&mut central, 0);
        u32le(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    u32le(&mut out, 0x0605_4b50);
    for x in [0u16, 0, files.len() as u16, files.len() as u16] {
        u16le(&mut out, x);
    }
    u32le(&mut out, central.len() as u32);
    u32le(&mut out, cd_offset);
    u16le(&mut out, 0);
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A cell of the generated sheet.
enum Cell<'a> {
    Num(f64),
    Text(&'a str),
}

/// Writes a workbook with one sheet "Captions"; `first_col` is the 0-based column of the first cell of every row.
fn write_xlsx(path: &Path, first_col: usize, rows: &[Vec<Cell>]) {
    let mut strings: Vec<String> = Vec::new();
    let mut sheet_rows = String::new();
    for (r, row) in rows.iter().enumerate() {
        sheet_rows.push_str(&format!("<row r=\"{}\">", r + 1));
        for (c, cell) in row.iter().enumerate() {
            let r_ref = format!("{}{}", crate::sheet::column_letters(first_col + c), r + 1);
            match cell {
                Cell::Num(n) => sheet_rows.push_str(&format!("<c r=\"{r_ref}\"><v>{n}</v></c>")),
                Cell::Text(t) => {
                    let i = strings.iter().position(|s| s == t).unwrap_or_else(|| {
                        strings.push((*t).to_owned());
                        strings.len() - 1
                    });
                    sheet_rows.push_str(&format!("<c r=\"{r_ref}\" t=\"s\"><v>{i}</v></c>"));
                }
            }
        }
        sheet_rows.push_str("</row>");
    }
    let sst: String = strings.iter().map(|s| format!("<si><t xml:space=\"preserve\">{}</t></si>", esc(s))).collect();
    let ns = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    let rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    let files = vec![
        ("[Content_Types].xml", "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/><Override PartName=\"/xl/sharedStrings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml\"/></Types>".to_owned()),
        ("_rels/.rels", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{rel}/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>")),
        ("xl/workbook.xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><workbook xmlns=\"{ns}\" xmlns:r=\"{rel}\"><sheets><sheet name=\"Captions\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>")),
        ("xl/_rels/workbook.xml.rels", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{rel}/worksheet\" Target=\"worksheets/sheet1.xml\"/><Relationship Id=\"rId2\" Type=\"{rel}/sharedStrings\" Target=\"sharedStrings.xml\"/></Relationships>")),
        ("xl/sharedStrings.xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><sst xmlns=\"{ns}\" count=\"{n}\" uniqueCount=\"{n}\">{sst}</sst>", n = strings.len())),
        ("xl/worksheets/sheet1.xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><worksheet xmlns=\"{ns}\"><sheetData>{sheet_rows}</sheetData></worksheet>")),
    ];
    std::fs::write(path, zip_stored(&files)).unwrap();
}

fn demo_rows() -> Vec<Vec<Cell<'static>>> {
    vec![
        vec![Cell::Text("Nr"), Cell::Text("Dateiname"), Cell::Text("Beschriftung")],
        vec![Cell::Num(75.0), Cell::Text("075_Anna Berger"), Cell::Text("Anna Berger, Tirol")],
        vec![Cell::Num(76.0), Cell::Text("076_Müller & Söhne"), Cell::Text("Team Müller & Söhne")],
        vec![Cell::Num(77.0), Cell::Text("077_Müller & Söhne"), Cell::Text("Team Müller & Söhne")],
    ]
}

#[test]
fn reads_a_generated_xlsx_with_default_columns() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("captions.xlsx");
    write_xlsx(&p, 0, &demo_rows());
    let s = CaptionSheet::load(&p, &SheetColumns::default(), None).unwrap();
    assert_eq!((s.header_rows, s.len()), (1, 3));
    assert_eq!(s.lookup(76).unwrap().name, "076_Müller & Söhne");
    assert_eq!(s.lookup(75).unwrap().caption, "Anna Berger, Tirol");
    assert_eq!(s.blocks().len(), 2, "Anna alone, then the Müller block of two");
    // the named sheet (case-insensitive) and a wrong name
    assert!(CaptionSheet::load(&p, &SheetColumns::default(), Some("captions")).is_ok());
    assert!(CaptionSheet::load(&p, &SheetColumns::default(), Some("nope")).is_err());
}

#[test]
fn a_sheet_that_starts_in_column_b_keeps_absolute_columns() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("shifted.xlsx");
    write_xlsx(&p, 1, &demo_rows());
    let s = CaptionSheet::load(&p, &SheetColumns { number: 1, name: 2, caption: 3 }, None).unwrap();
    assert_eq!(s.lookup(77).unwrap().name, "077_Müller & Söhne");
    // With the defaults the number column is empty: no usable row.
    assert_eq!(CaptionSheet::load(&p, &SheetColumns::default(), None), Err(crate::sheet::SheetError::NoRows));
}

#[test]
fn csv_and_garbage_files() {
    let t = tempfile::tempdir().unwrap();
    let c = t.path().join("c.csv");
    std::fs::write(&c, "Nr;Name;Caption\n76;076_Anna;Anna\n").unwrap();
    assert_eq!(CaptionSheet::load(&c, &SheetColumns::default(), None).unwrap().lookup(76).unwrap().name, "076_Anna");
    let bad = t.path().join("bad.xlsx");
    std::fs::write(&bad, "not a zip").unwrap();
    assert!(matches!(CaptionSheet::load(&bad, &SheetColumns::default(), None), Err(crate::sheet::SheetError::Format(_))));
    assert!(matches!(CaptionSheet::load(&t.path().join("x.pdf"), &SheetColumns::default(), None), Err(crate::sheet::SheetError::Format(_))));
    assert!(matches!(CaptionSheet::load(&t.path().join("missing.csv"), &SheetColumns::default(), None), Err(crate::sheet::SheetError::Io(_))));
}
