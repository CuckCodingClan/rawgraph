//! HEIC through the operating system's encoder (no HEVC encoder is bundled: patents / licences).
//!
//! * Windows: Windows Imaging Component (WIC) HEIF container. It works while the "HEVC Video
//!   Extensions" (and the "HEIF Image Extensions") from the Microsoft Store are installed; without
//!   them [`heic_available`] says so. 8 bits per channel, quality, the ICC profile of the output
//!   space and EXIF (best effort, see `write_exif`) are written. The file gets a `colr nclx`
//!   property (BT.709 matrix, full range) because WIC does not signal the matrix it uses and other
//!   decoders would assume BT.601 (visible colour shifts); sizes the encoder rejects (tile
//!   remainders of 2 to 10 pixels above 4096) are encoded on a canvas padded to a multiple of 64
//!   and cropped with a `clap` property.
//! * macOS / Linux: not available (clear message). A macOS ImageIO implementation could not be
//!   compiled and checked on the build machine, so none is shipped.
//!
//! The decoder here exists for the Export preview and the tests; HEIC *import* is out of scope.

/// Message shown when the Windows encoder is missing.
pub const HEIC_MISSING_MESSAGE: &str = "HEIC needs the HEVC Video Extensions from the Microsoft Store (or use AVIF)";

/// A decoded HEIC picture (8-bit RGB) with what the file declares about colour and metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct HeicImage {
    pub width: u32,
    pub height: u32,
    /// RGB, 3 bytes per pixel.
    pub rgb: Vec<u8>,
    /// Embedded ICC profile, if the container carries one.
    pub icc: Option<Vec<u8>>,
    /// EXIF `Make` read back through the metadata query reader (proves EXIF was stored).
    pub make: Option<String>,
}

/// Whether HEIC can be written here. `Err` holds the reason, ready to show next to a greyed-out
/// option. The answer comes from a real trial encode of a picture; a success is cached for the
/// process, a failure is not (installing the extensions takes effect without a restart).
pub fn heic_available() -> Result<(), String> {
    imp::available()
}

/// Smallest side (pixels) the operating system's HEIC encoder handles reliably: below it the
/// Windows encoder fails, writes files that cannot be decoded, or ends the whole process.
pub const HEIC_MIN_SIDE: u32 = 16;

/// A finished HEIC file and how much of the requested EXIF the encoder accepted.
#[derive(Debug, Clone, PartialEq)]
pub struct HeicEncoded {
    pub bytes: Vec<u8>,
    /// EXIF entries the encoder accepted / that were requested.
    pub exif_written: usize,
    pub exif_requested: usize,
}

/// The ICC profile of a HEIF/HEIC file: payload of the first `colr` box of type `prof`/`rICC`
/// (ISO/IEC 23008-12), found by scanning the container.
pub fn embedded_icc(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut from = 0usize;
    while let Some(rel) = bytes.get(from..)?.windows(4).position(|w| w == b"colr") {
        let at = from + rel;
        from = at + 4;
        if at < 4 || bytes.len() < at + 8 {
            continue;
        }
        let size = u32::from_be_bytes([bytes[at - 4], bytes[at - 3], bytes[at - 2], bytes[at - 1]]) as usize;
        let kind = &bytes[at + 4..at + 8];
        if (kind == b"prof" || kind == b"rICC") && size > 12 {
            return bytes.get(at + 8..at - 4 + size).map(<[u8]>::to_vec);
        }
    }
    None
}

pub(crate) use imp::{decode, encode};

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;

    use rawgraph_core::color::OutputSpace;
    use windows::core::{Interface, PCWSTR, PSTR, PWSTR};
    use windows::Win32::Foundation::{E_INVALIDARG, HGLOBAL, RPC_E_CHANGED_MODE};
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, IPropertyBag2, PROPBAG2, PROPVARIANT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoUninitialize, IStream, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
        STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
    };
    use windows::Win32::System::Variant::{VARIANT, VT_LPSTR, VT_R4};

    use super::{HeicEncoded, HeicImage, HEIC_MISSING_MESSAGE};
    use crate::heif_box::{add_primary_properties, clap_box, nclx_box, NewProp};
    use crate::icc::icc_profile;
    use crate::meta::ExportMetadata;
    use crate::IoError;

    /// Initialises COM for the calling thread for the lifetime of the guard.
    struct Com(bool);

    impl Com {
        fn new() -> Com {
            // S_OK / S_FALSE: initialised (balance with CoUninitialize); a thread that already
            // uses another apartment model keeps it and is not touched.
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            Com(hr.is_ok() && hr != RPC_E_CHANGED_MODE)
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn werr(what: &str, e: windows::core::Error) -> IoError {
        IoError::Io(format!("HEIC ({what}): {e}"))
    }

    pub fn available() -> Result<(), String> {
        static OK: OnceLock<()> = OnceLock::new();
        if OK.get().is_some() {
            return Ok(());
        }
        let rgb = [200u8, 120, 40].repeat(16 * 16);
        match encode(16, 16, &rgb, 90, None, None, false) {
            Ok(e) if e.bytes.len() > 16 => {
                let _ = OK.set(());
                Ok(())
            }
            Ok(_) => Err("The Windows HEIC encoder returned an empty file".to_string()),
            Err(IoError::Unsupported(why)) => Err(why),
            Err(e) => Err(e.to_string()),
        }
    }

    fn read_stream(stream: &IStream) -> Result<Vec<u8>, IoError> {
        unsafe {
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME).map_err(|e| werr("stream size", e))?;
            let len = usize::try_from(stat.cbSize).map_err(|_| IoError::Io("HEIC: stream too large".into()))?;
            stream.Seek(0, STREAM_SEEK_SET, None).map_err(|e| werr("stream seek", e))?;
            let mut out = vec![0u8; len];
            let mut done = 0usize;
            while done < len {
                let mut got = 0u32;
                let chunk = (len - done).min(1 << 26) as u32;
                stream.Read(out[done..].as_mut_ptr().cast(), chunk, Some(&mut got)).ok().map_err(|e| werr("stream read", e))?;
                if got == 0 {
                    break;
                }
                done += got as usize;
            }
            out.truncate(done);
            Ok(out)
        }
    }

    fn set_quality(bag: &IPropertyBag2, quality: u8) -> windows::core::Result<()> {
        let mut name = wide("ImageQuality");
        let prop = PROPBAG2 { pstrName: PWSTR(name.as_mut_ptr()), vt: VT_R4, ..Default::default() };
        let value = VARIANT::from(f32::from(quality.clamp(1, 100)) / 100.0);
        unsafe { bag.Write(1, &prop, &value) }
    }

    /// `VT_LPSTR` value (what EXIF ASCII tags need; `PROPVARIANT::from(&str)` gives `VT_LPWSTR`,
    /// which WIC rejects). The text is UTF-8, NUL-terminated and freed by the PROPVARIANT.
    fn lpstr(s: &str) -> Option<PROPVARIANT> {
        let bytes: Vec<u8> = s.bytes().filter(|b| *b != 0).chain(std::iter::once(0)).collect();
        unsafe {
            let mem = CoTaskMemAlloc(bytes.len()).cast::<u8>();
            if mem.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mem, bytes.len());
            let mut pv = PROPVARIANT::default();
            let inner = &mut *pv.Anonymous.Anonymous;
            inner.vt = VT_LPSTR;
            inner.Anonymous.pszVal = PSTR(mem);
            Some(pv)
        }
    }

    fn set(writer: &IWICMetadataQueryWriter, name: &str, value: &PROPVARIANT) -> bool {
        let n = wide(name);
        unsafe { writer.SetMetadataByName(PCWSTR(n.as_ptr()), value).is_ok() }
    }

    /// Best-effort EXIF through the frame's metadata query writer: make, model, lens, artist,
    /// copyright, description, software, capture date, exposure time, f-number, focal length and ISO.
    /// GPS is not written (it needs rational vectors that this path does not build). Returns how
    /// many entries the encoder accepted.
    pub(super) fn write_exif(writer: &IWICMetadataQueryWriter, md: &ExportMetadata) -> (usize, usize) {
        let mut ok = 0usize;
        let mut req = 0usize;
        let mut text = |name: &str, v: &Option<String>| {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                req += 1;
                if let Some(pv) = lpstr(v) { ok += usize::from(set(writer, name, &pv)); }
            }
        };
        text("/ifd/{ushort=271}", &md.make);
        text("/ifd/{ushort=272}", &md.model);
        text("/ifd/{ushort=315}", &md.artist);
        text("/ifd/{ushort=33432}", &md.copyright);
        text("/ifd/{ushort=270}", &md.description);
        text("/ifd/{ushort=305}", &Some(md.software.clone().unwrap_or_else(|| "RawGraph".into())));
        text("/ifd/exif/{ushort=36867}", &md.datetime_original);
        text("/ifd/exif/{ushort=42036}", &md.lens);
        let rational = |(n, d): (u32, u32)| PROPVARIANT::from((u64::from(d) << 32) | u64::from(n));
        if let Some(r) = md.exposure_time {
            req += 1;
            ok += usize::from(set(writer, "/ifd/exif/{ushort=33434}", &rational(r)));
        }
        if let Some(r) = md.f_number {
            req += 1;
            ok += usize::from(set(writer, "/ifd/exif/{ushort=33437}", &rational(r)));
        }
        if let Some(r) = md.focal_length {
            req += 1;
            ok += usize::from(set(writer, "/ifd/exif/{ushort=37386}", &rational(r)));
        }
        if let Some(iso) = md.iso {
            req += 1;
            ok += usize::from(set(writer, "/ifd/exif/{ushort=34855}", &PROPVARIANT::from(iso.min(65535) as u16)));
        }
        (ok, req)
    }

    /// Errors that mean "the encoder is not installed", as opposed to a failure for this picture.
    fn is_missing(code: i32) -> bool {
        // WINCODEC_ERR_COMPONENTNOTFOUND, WINCODEC_ERR_UNSUPPORTEDOPERATION-style codec lookups,
        // REGDB_E_CLASSNOTREG, CO_E_CLASSSTRING.
        matches!(code as u32, 0x88982F50 | 0x88982F8B | 0x80040154 | 0x800401F3)
    }

    /// One encode with WIC at exactly this size. The picture is stored as is (no container fix-ups).
    fn encode_once(
        width: u32, height: u32, rgb: &[u8], quality: u8, space: Option<OutputSpace>, exif: Option<&ExportMetadata>, with_icc: bool,
    ) -> windows::core::Result<(Vec<u8>, usize, usize)> {
        unsafe {
            let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
            let encoder = factory.CreateEncoder(&GUID_ContainerFormatHeif, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame: Option<IWICBitmapFrameEncode> = None;
            let mut bag: Option<IPropertyBag2> = None;
            encoder.CreateNewFrame(&mut frame, &mut bag)?;
            let frame = frame.ok_or_else(windows::core::Error::empty)?;
            if let Some(bag) = &bag {
                set_quality(bag, quality)?;
            }
            frame.Initialize(bag.as_ref())?;
            frame.SetSize(width, height)?;
            frame.SetResolution(96.0, 96.0)?;
            let mut format = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut format)?;
            let pixels: Vec<u8> = if format == GUID_WICPixelFormat24bppBGR {
                rgb.as_chunks::<3>().0.iter().flat_map(|p| [p[2], p[1], p[0]]).collect()
            } else if format == GUID_WICPixelFormat24bppRGB {
                rgb.to_vec()
            } else if format == GUID_WICPixelFormat32bppBGRA || format == GUID_WICPixelFormat32bppBGR {
                rgb.as_chunks::<3>().0.iter().flat_map(|p| [p[2], p[1], p[0], 255]).collect()
            } else {
                return Err(windows::core::Error::new(windows::core::HRESULT(0x88982F80u32 as i32), "the HEIC encoder wants an unsupported pixel format"));
            };
            if with_icc {
                if let Some(space) = space {
                    let cc = factory.CreateColorContext()?;
                    cc.InitializeFromMemory(icc_profile(space))?;
                    frame.SetColorContexts(&[Some(cc)])?;
                }
            }
            let (mut written, mut requested) = (0, 0);
            if let Some(md) = exif {
                if let Ok(writer) = frame.GetMetadataQueryWriter() {
                    (written, requested) = write_exif(&writer, md);
                }
            }
            let stride = (pixels.len() / height as usize) as u32;
            frame.WritePixels(height, stride, &pixels)?;
            frame.Commit()?;
            encoder.Commit()?;
            let bytes = read_stream(&stream).map_err(|e| windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), e.to_string()))?;
            Ok((bytes, written, requested))
        }
    }

    /// Repeats the last row and column so the picture fills `padded`.
    fn pad(rgb: &[u8], (w, h): (u32, u32), (pw, ph): (u32, u32)) -> Vec<u8> {
        let (w, h, pw, ph) = (w as usize, h as usize, pw as usize, ph as usize);
        let mut out = Vec::with_capacity(pw * ph * 3);
        for y in 0..ph {
            let row = &rgb[y.min(h - 1) * w * 3..][..w * 3];
            out.extend_from_slice(row);
            let last = &row[(w - 1) * 3..];
            for _ in w..pw {
                out.extend_from_slice(last);
            }
        }
        out
    }

    /// Encodes 8-bit RGB. `exif`: metadata to try to store. `with_icc` embeds `space`'s profile.
    ///
    /// Beyond what WIC writes: a `colr nclx` property states the colour coding (BT.709 matrix, full
    /// range, primaries and transfer of the output space), because without it other decoders
    /// assume BT.601 and shift colours. If WIC rejects the size (`E_INVALIDARG`: tile remainders of
    /// 2 to 10 pixels above 4096 px) the picture is encoded again on a canvas padded by repeating
    /// the edge pixels to a multiple of 64, and a `clap` property crops it back to the real size.
    pub fn encode(width: u32, height: u32, rgb: &[u8], quality: u8, space: Option<OutputSpace>, exif: Option<&ExportMetadata>, with_icc: bool) -> Result<HeicEncoded, IoError> {
        if width < super::HEIC_MIN_SIDE || height < super::HEIC_MIN_SIDE {
            return Err(IoError::Unsupported(format!("HEIC needs at least {0} pixels on each side (this picture is {width}x{height})", super::HEIC_MIN_SIDE)));
        }
        let _com = Com::new();
        let rejected = |e: windows::core::Error| {
            if is_missing(e.code().0) {
                IoError::Unsupported(HEIC_MISSING_MESSAGE.into())
            } else {
                IoError::Unsupported(format!("The Windows HEVC encoder rejected {width}x{height} (0x{:08X}: {})", e.code().0 as u32, e.message()))
            }
        };
        let mut canvas = None;
        let (bytes, exif_written, exif_requested) = match encode_once(width, height, rgb, quality, space, exif, with_icc) {
            Ok(r) => r,
            Err(e) if e.code() == E_INVALIDARG && (!width.is_multiple_of(64) || !height.is_multiple_of(64)) => {
                let padded = (width.div_ceil(64) * 64, height.div_ceil(64) * 64);
                canvas = Some(padded);
                encode_once(padded.0, padded.1, &pad(rgb, (width, height), padded), quality, space, exif, with_icc).map_err(rejected)?
            }
            Err(e) => return Err(rejected(e)),
        };
        let (primaries, transfer) = crate::avif::cicp(space.unwrap_or(OutputSpace::Srgb)).map_or((2, 2), |(p, t)| (u16::from(p), u16::from(t)));
        let mut bytes = bytes;
        let mut props = vec![NewProp { data: nclx_box(primaries, transfer, 1, true), essential: false }];
        if let Some(c) = canvas {
            // A tile grid states the real size itself; anything else is cropped with `clap`.
            let grid = crate::heif_box::crop_grid_to(&mut bytes, c, (width, height)).map_err(|e| IoError::Io(format!("HEIC container: {e}")))?;
            if !grid {
                props.push(NewProp { data: clap_box(width, height, c), essential: true });
            }
        }
        let mut bytes = add_primary_properties(&bytes, &props).map_err(|e| IoError::Io(format!("HEIC container: {e}")))?;
        // ffmpeg and other decoders read the matrix from the bitstream's VUI, not from `colr`. If the
        // encoder wrote a parameter set this reader does not follow, the file still carries `nclx`
        // (correct for MIAF readers); only bitstream-only readers would then assume BT.601.
        if let Err(e) = crate::heif_box::patch_vui_colour(&mut bytes, primaries as u8, transfer as u8, 1, true) {
            eprintln!("rawgraph: HEIC colour signalling in the bitstream skipped: {e}");
        }
        // Tile grids (pictures larger than 2048 px) of some sizes (a side of 512*n + 1 px) are written
        // by the Windows encoder but cannot be read back by Windows itself: check before writing.
        if width.max(height) > 2048 {
            if let Err(e) = decode(&bytes) {
                return Err(IoError::Unsupported(format!(
                    "Windows cannot read back a HEIC of {width}x{height} pixels ({e}); crop or resize by one pixel, or use AVIF/JPEG"
                )));
            }
        }
        Ok(HeicEncoded { bytes, exif_written, exif_requested })
    }

    pub fn decode(bytes: &[u8]) -> Result<HeicImage, IoError> {
        let _com = Com::new();
        let run = || -> windows::core::Result<HeicImage> {
            unsafe {
                let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
                let wic_stream = factory.CreateStream()?;
                wic_stream.InitializeFromMemory(bytes)?;
                let stream: IStream = wic_stream.cast()?;
                let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
                let frame = decoder.GetFrame(0)?;
                let (mut w, mut h) = (0u32, 0u32);
                frame.GetSize(&mut w, &mut h)?;
                let converter = factory.CreateFormatConverter()?;
                converter.Initialize(&frame, &GUID_WICPixelFormat24bppRGB, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
                let stride = w * 3;
                let mut rgb = vec![0u8; stride as usize * h as usize];
                converter.CopyPixels(std::ptr::null(), stride, &mut rgb)?;
                let icc = super::embedded_icc(bytes);
                let mut make = None;
                if let Ok(reader) = frame.GetMetadataQueryReader() {
                    let name = wide("/ifd/{ushort=271}");
                    let mut v = PROPVARIANT::default();
                    if reader.GetMetadataByName(PCWSTR(name.as_ptr()), &mut v).is_ok() {
                        make = Some(v.to_string()).filter(|s| !s.is_empty());
                    }
                }
                Ok(HeicImage { width: w, height: h, rgb, icc, make })
            }
        };
        run().map_err(|e| IoError::Decode(format!("HEIC: {e}")))
    }
}

#[cfg(not(windows))]
mod imp {
    use rawgraph_core::color::OutputSpace;

    use super::{HeicEncoded, HeicImage};
    use crate::meta::ExportMetadata;
    use crate::IoError;

    const WHY: &str = if cfg!(target_os = "macos") {
        "HEIC export is not available in this build on macOS (use AVIF)"
    } else {
        "HEIC is not available on this platform (use AVIF)"
    };

    pub fn available() -> Result<(), String> {
        Err(WHY.to_string())
    }

    pub fn encode(_: u32, _: u32, _: &[u8], _: u8, _: Option<OutputSpace>, _: Option<&ExportMetadata>, _: bool) -> Result<HeicEncoded, IoError> {
        Err(IoError::Unsupported(WHY.into()))
    }

    pub fn decode(_: &[u8]) -> Result<HeicImage, IoError> {
        Err(IoError::Unsupported(WHY.into()))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use rawgraph_core::color::OutputSpace;

    use crate::heif_box::vui_colour;

    /// The colour description inside the bitstream (what ffmpeg reads) and in `colr` agree.
    #[test]
    fn bitstream_and_container_state_the_same_colour_coding() {
        if super::heic_available().is_err() {
            return;
        }
        for (space, want) in [(OutputSpace::Srgb, (1, 13)), (OutputSpace::DisplayP3, (12, 13)), (OutputSpace::Rec2020, (9, 14)), (OutputSpace::AdobeRgb, (2, 2))] {
            for (w, h) in [(64u32, 48u32), (4104, 600)] {
                let rgb = vec![120u8; (w * h * 3) as usize];
                let bytes = super::imp::encode(w, h, &rgb, 90, Some(space), None, true).unwrap().bytes;
                assert_eq!(vui_colour(&bytes), Some((want.0, want.1, 1, true)), "{space:?} {w}x{h}");
                let nclx = bytes.windows(8).position(|b| b == b"colrnclx").expect("colr nclx");
                assert_eq!(&bytes[nclx + 8..nclx + 15], &[0, u16::from(want.0) as u8, 0, want.1, 0, 1, 0x80], "{space:?} {w}x{h}");
                assert!(super::embedded_icc(&bytes).is_some(), "the ICC profile stays next to nclx");
            }
        }
    }
}
