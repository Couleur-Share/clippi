//! --- Shared platform utilities. ---

/// Encode raw RGBA pixel data as a PNG byte vector.
#[cfg(target_os = "windows")]
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    use image::{codecs::png::PngEncoder, ImageEncoder};
    let mut buf = Vec::new();
    let encoder = PngEncoder::new(&mut buf);
    encoder
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(buf)
}

/// Trim the process working set so the OS reclaims idle pages.
/// This is a hint — it resets WS limits and lets Windows evict pages
/// that are no longer actively used. Safe to call at any time.
#[cfg(target_os = "windows")]
pub fn trim_process_working_set() {
    // SAFETY: `SetProcessWorkingSetSizeEx` with `SIZE_T(-1, -1)` and no-flags
    // provides a trim hint to the OS on the current process. It can be called
    // from any thread and never fails in a way that corrupts state.
    unsafe {
        use windows_sys::Win32::System::Memory::SetProcessWorkingSetSizeEx;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0);
    }
}

#[cfg(target_os = "macos")]
pub fn trim_process_working_set() {
    use std::ffi::c_void;

    extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut c_void, goal: usize) -> usize;
    }

    // SAFETY: These macOS malloc APIs only provide a best-effort pressure
    // relief hint. A null zone asks malloc to examine all zones.
    unsafe {
        let _ = malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn trim_process_working_set() {
    // --- no-op on unsupported platforms ---
}

/// Render an HICON to a 32x32 BGRA DIB, convert to RGBA PNG, and base64-encode.
/// Shared by source app icon extraction and file icon extraction on Windows.
#[cfg(target_os = "windows")]
pub fn hicon_to_base64_png(
    hicon: windows_sys::Win32::Foundation::HANDLE,
    size: i32,
) -> Option<String> {
    use base64::Engine;
    use windows_sys::Win32::Foundation::{BOOL, HANDLE};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBRUSH, HDC,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon;

    extern "system" {
        fn DrawIconEx(
            hdc: HDC,
            xLeft: i32,
            yTop: i32,
            hIcon: HANDLE,
            cxWidth: i32,
            cyWidth: i32,
            istepIfAniCur: u32,
            hbrFlickerFreeDraw: HBRUSH,
            diFlags: u32,
        ) -> BOOL;
    }

    const DI_NORMAL: u32 = 0x0003;

    // SAFETY: GDI drawing calls (`GetDC`, `CreateCompatibleDC`, `CreateDIBSection`,
    // `DrawIconEx`) are safe when used with valid DCs and properly-initialised
    // BITMAPINFO. The raw pixel buffer (`pixels`) is owned by the DIB section
    // and copied via `from_raw_parts` before the DIB/DC are destroyed.
    // `DestroyIcon` takes ownership of `hicon` and is always called (even on
    // error paths). The BGRA→RGBA swizzle uses safe Rust slices.
    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        let mem_dc = CreateCompatibleDC(screen_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);

        if mem_dc.is_null() {
            DestroyIcon(hicon);
            return None;
        }

        let bmp_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [std::mem::zeroed(); 1],
        };

        let mut pixels: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            mem_dc,
            &bmp_info,
            DIB_RGB_COLORS,
            &mut pixels,
            std::ptr::null_mut(),
            0,
        );
        if dib.is_null() || pixels.is_null() {
            DeleteDC(mem_dc);
            DestroyIcon(hicon);
            return None;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        DrawIconEx(
            mem_dc,
            0,
            0,
            hicon,
            size,
            size,
            0,
            std::ptr::null_mut(),
            DI_NORMAL,
        );

        let pixel_count = (size * size) as usize;
        let bgra_data: Vec<u8> =
            std::slice::from_raw_parts(pixels as *const u8, pixel_count * 4).to_vec();

        SelectObject(mem_dc, old_bmp);
        DeleteObject(dib);
        DeleteDC(mem_dc);
        DestroyIcon(hicon);

        // --- BGRA → RGBA ---
        let mut rgba = Vec::with_capacity(pixel_count * 4);
        for chunk in bgra_data.chunks_exact(4) {
            rgba.push(chunk[2]); // R
            rgba.push(chunk[1]); // G
            rgba.push(chunk[0]); // B
            rgba.push(chunk[3]); // A
        }

        let png_bytes = encode_png(&rgba, size as u32, size as u32)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        Some(b64)
    }
}

/// Decode a byte run that a document escaped as single-byte `\'hh` sequences.
///
/// `codepage` is the document's declared ANSI code page (`\ansicpgN` in RTF),
/// or 0 when it is unknown and the system default should be used. RichEdit-based
/// Windows sources (Event Viewer, WordPad, most Win32 rich-edit controls) escape
/// CJK text in the system code page — cp936 for Chinese Windows — so the bytes
/// must go through the code page instead of being mapped one-to-one to scalars,
/// which would produce mojibake such as `´íÎó` for `错误` (issue #114).
#[cfg(target_os = "windows")]
pub fn decode_ansi_bytes(codepage: u16, bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{IsValidCodePage, MultiByteToWideChar, CP_ACP};

    if bytes.is_empty() {
        return String::new();
    }
    // SAFETY: `IsValidCodePage` only inspects its value argument.
    let code_page_is_known = codepage != 0 && unsafe { IsValidCodePage(codepage as u32) } != 0;
    let page = if code_page_is_known {
        codepage as u32
    } else {
        CP_ACP
    };

    // SAFETY: `MultiByteToWideChar` reads exactly `bytes.len()` bytes from the
    // input pointer and writes at most `wide.len()` UTF-16 units into the
    // output buffer. Both calls use the same input, and the second call is only
    // made when the first reported a positive unit count.
    unsafe {
        let needed = MultiByteToWideChar(
            page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if needed <= 0 {
            return latin1_to_string(bytes);
        }
        let mut wide = vec![0u16; needed as usize];
        let written = MultiByteToWideChar(
            page,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            needed,
        );
        if written <= 0 {
            return latin1_to_string(bytes);
        }
        wide.truncate(written as usize);
        String::from_utf16_lossy(&wide)
    }
}

/// Best-effort counterpart of [`decode_ansi_bytes`] for platforms without a
/// code-page API. Byte values are mapped to the matching Latin-1 scalar, which
/// is exact for ASCII and for the RTF default code page 1252 letters/accents,
/// and approximate elsewhere. UTF-8 (65001) is decoded exactly everywhere.
///
/// TODO(macOS): resolve the declared code page instead of approximating it, so
/// an RTF entry captured on Windows (cp936/cp950/cp932/cp949/1251 — the code
/// page is already resolved for us by `core::rtf_text::initial_codepage`) keeps
/// its text after syncing to a Mac. To be done on a Mac:
///
/// 1. `CFStringConvertWindowsCodepageToEncoding(codepage as u32)` returns the
///    matching `CFStringEncoding`; treat `kCFStringEncodingInvalidId` as
///    "unknown" and keep the Latin-1 fallback.
/// 2. `CFStringCreateWithBytes(null, bytes.as_ptr(), bytes.len() as isize,
///    encoding, false)` builds the string. Convert it to Rust with
///    `CFStringGetMaximumSizeForEncoding` + `CFStringGetCString(..,
///    kCFStringEncodingUTF8)` into a `Vec<u8>`, or wrap the ref in an
///    `NSString` through `objc2-foundation` and read its UTF-8 bytes.
/// 3. `CFRelease` the `CFStringRef` on both the success and the failure path,
///    and fall back to [`latin1_to_string`] whenever either step fails.
///
/// CoreFoundation is already linked (`core-foundation` /
/// `objc2-core-foundation` are dependencies) and `core::rtf_text` hands this
/// function a plain numeric code page, so no RTF-side change is needed.
/// Verification: `core::rtf_text::tests` asserts cp936 decoding in
/// `gbk_byte_run_decodes_through_the_document_code_page` and
/// `missing_ansicpg_decodes_through_the_font_charset`; both are
/// `#[cfg(target_os = "windows")]` only because this fallback is approximate —
/// drop the gates and run `cargo test rtf` on the Mac to prove the adaptation.
#[cfg(not(target_os = "windows"))]
pub fn decode_ansi_bytes(codepage: u16, bytes: &[u8]) -> String {
    if codepage == 65001 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    latin1_to_string(bytes)
}

/// Map raw bytes to the Latin-1 scalars with the same values (never fails).
fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().copied().map(char::from).collect()
}

// macOS geometry types for use with objc2 Encode (available for future needs).
// Kept for potential use with CoreGraphics rendering paths.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
mod macos_geometry {
    use objc2::encode::{Encode, Encoding, RefEncode};

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CGPoint {
        pub x: f64,
        pub y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CGSize {
        pub width: f64,
        pub height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CGRect {
        pub origin: CGPoint,
        pub size: CGSize,
    }

    impl CGPoint {
        pub const fn new(x: f64, y: f64) -> Self {
            Self { x, y }
        }
    }

    impl CGSize {
        pub const fn new(width: f64, height: f64) -> Self {
            Self { width, height }
        }
    }

    impl CGRect {
        pub const fn new(origin: CGPoint, size: CGSize) -> Self {
            Self { origin, size }
        }
    }

    // --- f64 = CGFloat on 64-bit macOS → encoding "d" ---
    unsafe impl Encode for CGPoint {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl RefEncode for CGPoint {
        const ENCODING_REF: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }

    unsafe impl Encode for CGSize {
        const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }
    unsafe impl RefEncode for CGSize {
        const ENCODING_REF: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
    }

    unsafe impl Encode for CGRect {
        const ENCODING: Encoding =
            Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
    }
    unsafe impl RefEncode for CGRect {
        const ENCODING_REF: Encoding =
            Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
    }
}

/// Convert an NSImage to a base64-encoded PNG (no resize — caller can scale via UI).
///
/// Uses TIFF → NSBitmapImageRep → PNG pipeline which is thread-safe
/// (unlike lockFocus/unlockFocus which require the main thread).
/// GPUI's `img()` element handles display sizing via `object_fit`.
#[cfg(target_os = "macos")]
pub fn nsimage_to_base64_png(image: &objc2_app_kit::NSImage, _size: i32) -> Option<String> {
    unsafe {
        use std::ffi::{c_char, c_void, CStr};

        extern "C" {
            fn clippi_nsimage_to_png_base64(image: *const c_void, target_size: i32) -> *mut c_char;
            fn clippi_ocr_free_string(s: *mut c_char);
        }

        let ptr = clippi_nsimage_to_png_base64(image as *const _ as *const c_void, _size);
        if ptr.is_null() {
            return None;
        }
        let result = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        clippi_ocr_free_string(ptr);
        Some(result)
    }
}
