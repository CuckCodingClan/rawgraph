//! HEIC through the operating system's encoder (no HEVC encoder is bundled: patents / licences).
//!
//! * Windows: Windows Imaging Component (WIC) HEIF container. It works while the "HEVC Video
//!   Extensions" (and the "HEIF Image Extensions") from the Microsoft Store are installed; without
//!   them [`heic_available`] says so. 8 bits per channel, quality, the ICC profile of the output
//!   space and EXIF (best effort, see `write_exif`) are written.
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
/// option. The answer comes from a real trial encode of a tiny picture and is cached per process.
pub fn heic_available() -> Result<(), String> {
    imp::available()
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
    use windows::Win32::Foundation::{HGLOBAL, RPC_E_CHANGED_MODE};
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, IPropertyBag2, PROPBAG2, PROPVARIANT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoUninitialize, IStream, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
        STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
    };
    use windows::Win32::System::Variant::{VARIANT, VT_LPSTR, VT_R4};

    use super::{HeicImage, HEIC_MISSING_MESSAGE};
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
        static ANSWER: OnceLock<Result<(), String>> = OnceLock::new();
        ANSWER
            .get_or_init(|| {
                let rgb = [200u8, 120, 40].repeat(16 * 16);
                match encode(16, 16, &rgb, 90, None, None, false) {
                    Ok(bytes) if bytes.len() > 16 => Ok(()),
                    Ok(_) => Err(HEIC_MISSING_MESSAGE.to_string()),
                    Err(_) => Err(HEIC_MISSING_MESSAGE.to_string()),
                }
            })
            .clone()
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
    pub(super) fn write_exif(writer: &IWICMetadataQueryWriter, md: &ExportMetadata) -> usize {
        let mut ok = 0usize;
        let mut text = |name: &str, v: &Option<String>| {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
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
            ok += usize::from(set(writer, "/ifd/exif/{ushort=33434}", &rational(r)));
        }
        if let Some(r) = md.f_number {
            ok += usize::from(set(writer, "/ifd/exif/{ushort=33437}", &rational(r)));
        }
        if let Some(r) = md.focal_length {
            ok += usize::from(set(writer, "/ifd/exif/{ushort=37386}", &rational(r)));
        }
        if let Some(iso) = md.iso {
            ok += usize::from(set(writer, "/ifd/exif/{ushort=34855}", &PROPVARIANT::from(iso.min(65535) as u16)));
        }
        ok
    }

    /// Encodes 8-bit RGB. `exif`: metadata to try to store. `with_icc` embeds `space`'s profile.
    pub fn encode(width: u32, height: u32, rgb: &[u8], quality: u8, space: Option<OutputSpace>, exif: Option<&ExportMetadata>, with_icc: bool) -> Result<Vec<u8>, IoError> {
        let _com = Com::new();
        let run = || -> windows::core::Result<Vec<u8>> {
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
                    let _ = set_quality(bag, quality);
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
                        if let Ok(cc) = factory.CreateColorContext() {
                            if cc.InitializeFromMemory(icc_profile(space)).is_ok() {
                                let _ = frame.SetColorContexts(&[Some(cc)]);
                            }
                        }
                    }
                }
                if let Some(md) = exif {
                    if let Ok(writer) = frame.GetMetadataQueryWriter() {
                        write_exif(&writer, md);
                    }
                }
                let stride = (pixels.len() / height as usize) as u32;
                frame.WritePixels(height, stride, &pixels)?;
                frame.Commit()?;
                encoder.Commit()?;
                let bytes = read_stream(&stream).map_err(|_| windows::core::Error::empty())?;
                Ok(bytes)
            }
        };
        run().map_err(|e| {
            if e.code().0 as u32 == 0x88982F50 || e.code().0 as u32 == 0x88982F8B || e.code().0 as u32 == 0x80040154 {
                IoError::Unsupported(HEIC_MISSING_MESSAGE.into())
            } else {
                IoError::Unsupported(format!("{HEIC_MISSING_MESSAGE} ({e})"))
            }
        })
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

    use super::HeicImage;
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

    pub fn encode(_: u32, _: u32, _: &[u8], _: u8, _: Option<OutputSpace>, _: Option<&ExportMetadata>, _: bool) -> Result<Vec<u8>, IoError> {
        Err(IoError::Unsupported(WHY.into()))
    }

    pub fn decode(_: &[u8]) -> Result<HeicImage, IoError> {
        Err(IoError::Unsupported(WHY.into()))
    }
}
