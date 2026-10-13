//! Embedded preview extraction: the largest baseline/progressive JPEG stored in a TIFF-based raw (IFD strips
//! with JPEG compression, `JPEGInterchangeFormat` pointers in any IFD, Nikon/others' maker-note preview IFDs),
//! or a DNG 1.7 JPEG XL preview IFD stored as a single tile/strip (a standalone `.jxl` file, as the spec
//! recommends for previews).

use std::collections::HashSet;

use lightcraft_tiff::image::chunk_bytes;
use lightcraft_tiff::tags as t;
use lightcraft_tiff::{ByteOrder, Ifd, Tiff, makernote};

/// Colour space of an embedded JPEG when the enclosing raw supplies it instead of the JPEG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewColorSpace {
    Srgb,
    AdobeRgb,
}

/// The colour space the raw says its embedded JPEG is in, for a JPEG that declares none itself (no ICC profile,
/// no EXIF), first available first: the Nikon maker-note ColorSpace (0x001e: 1 = sRGB, 2 = Adobe RGB, per Nikon
/// tag documentation); the raw's own EXIF ColorSpace (0xA001: 1 = sRGB), or Uncalibrated (0xFFFF) with the
/// Interoperability IFD's InteropIndex "R03", the DCF option file for Adobe RGB (as a Sony body shot in Adobe RGB
/// writes, issue #778; per the Exif and DCF specifications), not for a DNG. This is a fallback only: a JPEG's own ICC/EXIF colour
/// declaration takes precedence.
pub fn embedded_preview_color_space(bytes: &[u8]) -> Option<PreviewColorSpace> {
    let tiff = Tiff::parse(bytes).ok()?;
    let make = tiff.ifds.first()?.string(t::MAKE).unwrap_or_default();
    let exif = tiff.exif()?;
    if make.to_ascii_uppercase().starts_with("NIKON")
        && let Some(space) = nikon_color_space(bytes, &tiff, exif, &make)
    {
        return Some(space);
    }
    // a DNG's EXIF describes the original capture; its previews are rendered by the converter
    if tiff.ifds.first()?.contains(t::DNG_VERSION) {
        return None;
    }
    match exif.u16(0xA001)? {
        1 => Some(PreviewColorSpace::Srgb),
        0xFFFF if exif.interop.as_deref()?.bytes(0x0001)?.starts_with(b"R03") => Some(PreviewColorSpace::AdobeRgb),
        _ => None,
    }
}

fn nikon_color_space(bytes: &[u8], tiff: &Tiff, exif: &Ifd, make: &str) -> Option<PreviewColorSpace> {
    let e = exif.get(t::MAKER_NOTE)?;
    let mn = makernote::parse_makernote(bytes, e.offset, e.count() as u64, tiff.order, make)?;
    match mn.ifd.u16(0x001e)? {
        1 => Some(PreviewColorSpace::Srgb),
        2 => Some(PreviewColorSpace::AdobeRgb),
        _ => None,
    }
}

/// Whether the camera optimised its embedded JPEG's dynamic range with local tone mapping that the raw data doesn't
/// carry: Sony's Dynamic Range Optimizer (maker note `0xb025`, see `vendor::arw`). Such a JPEG is brighter in its
/// darker regions than the raw developed with the camera's global tone curve. `Some(false)` when the file says it
/// was off, `None` when it doesn't say (other makers, notes without the tag, undocumented values).
pub fn embedded_preview_dynamic_range_optimized(bytes: &[u8]) -> Option<bool> {
    let tiff = Tiff::parse(bytes).ok()?;
    let make = tiff.ifds.first()?.string(t::MAKE)?;
    if !make.trim().to_ascii_uppercase().starts_with("SONY") {
        return None;
    }
    let e = tiff.exif()?.get(t::MAKER_NOTE)?;
    let mn = makernote::parse_makernote(bytes, e.offset, e.count() as u64, tiff.order, &make)?;
    crate::vendor::arw::dynamic_range_optimizer(&mn)
}

/// Whether `b` looks like a displayable (DCT) JPEG: SOI, and the first SOF marker is not lossless.
fn is_dct_jpeg(b: &[u8]) -> bool {
    if b.len() < 4 || b[0] != 0xff || b[1] != 0xd8 {
        return false;
    }
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xff {
            return false;
        }
        let m = b[i + 1];
        if m == 0xff {
            i += 1;
            continue;
        }
        match m {
            0xc0..=0xc2 => return true,
            0xc3 | 0xc5..=0xc7 | 0xcb | 0xcd..=0xcf => return false,
            0xda | 0xd9 => return false,
            _ => {}
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        i += 2 + len;
    }
    false
}

/// Whether `b` is a JPEG XL file: a bare codestream or the ISO-BMFF container's signature box.
fn is_jxl(b: &[u8]) -> bool {
    b.starts_with(&[0xff, 0x0a]) || b.starts_with(&[0, 0, 0, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a])
}

fn candidates<'a>(data: &'a [u8], ifd: &Ifd, base: u64, out: &mut Vec<&'a [u8]>) {
    // whole JPEG files stored as an undefined-type tag value (e.g. Panasonic `JpgFromRaw` 0x002e)
    for e in &ifd.entries {
        if matches!(e.value, lightcraft_tiff::Value::Undefined(_))
            && e.count() > 1024
            && let Some(s) = data.get(e.offset as usize..(e.offset.saturating_add(e.count() as u64) as usize).min(data.len()))
            && s.starts_with(&[0xff, 0xd8])
        {
            out.push(s);
        }
    }
    if let (Some(off), Some(len)) = (ifd.u64(t::JPEG_INTERCHANGE_FORMAT), ifd.u64(t::JPEG_INTERCHANGE_FORMAT_LENGTH)) {
        let off = off.saturating_add(base);
        if let Some(s) = data.get(off as usize..(off.saturating_add(len) as usize).min(data.len())) {
            out.push(s);
        }
    }
    if matches!(ifd.u16(t::COMPRESSION), Some(6) | Some(7) | Some(34892))
        && let Ok(info) = ifd.image()
    {
        let chunks = info.chunks(data.len() as u64);
        if chunks.len() == 1
            && let Some(s) = chunk_bytes(data, &chunks[0])
        {
            out.push(s);
        }
    }
    // a rendered (RGB or grey, never CFA/LinearRaw/mask) JPEG XL preview in one chunk
    if ifd.u16(t::COMPRESSION) == Some(t::compression::JPEG_XL)
        && matches!(ifd.u16(t::PHOTOMETRIC), Some(t::photometric::BLACK_IS_ZERO | t::photometric::RGB))
        && let Ok(info) = ifd.image()
        && let [chunk] = info.chunks(data.len() as u64).as_slice()
        && let Some(s) = chunk_bytes(data, chunk)
        && is_jxl(s)
    {
        out.push(s);
    }
}

/// The largest embedded preview, if any: a JPEG, or (DNG 1.7) a JPEG XL file — both decode with
/// `lightcraft_codecs::decode`.
pub fn embedded_preview(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.starts_with(b"FUJIFILMCCD-RAW") {
        let j = crate::vendor::raf::header(bytes).ok()?.jpeg?;
        return is_dct_jpeg(j).then(|| trim_eoi(j).to_vec());
    }
    let format = crate::probe(bytes);
    if format == Some(crate::RawFormat::Cr3) {
        return cr3_preview(bytes).map(|j| trim_eoi(j).to_vec());
    }
    if matches!(format, Some(crate::RawFormat::Crw | crate::RawFormat::Mrw | crate::RawFormat::X3f)) {
        return scan_for_jpeg(bytes);
    }
    let tiff = Tiff::parse(bytes).ok()?;
    let mut found: Vec<&[u8]> = Vec::new();
    for ifd in tiff.all_ifds() {
        candidates(bytes, ifd, 0, &mut found);
    }
    // maker-note preview IFDs (e.g. Nikon PreviewIFD 0x0011 holds JPEGInterchangeFormat relative to the note base)
    if let Some(exif) = tiff.exif()
        && let Some(e) = exif.get(t::MAKER_NOTE)
    {
        let make = tiff.find(t::MAKE).and_then(|e| e.value.as_str()).unwrap_or_default();
        if let Some(mn) = makernote::parse_makernote(bytes, e.offset, e.count() as u64, tiff.order, &make) {
            candidates(bytes, &mn.ifd, mn.base, &mut found);
            if let Some(off) = mn.ifd.u64(0x0011)
                && let Ok((pifd, _)) = lightcraft_tiff::parse_ifd_at(bytes, mn.base + off, mn.order, mn.base, false, &Default::default())
            {
                candidates(bytes, &pifd, mn.base, &mut found);
            }
        }
    }
    // Olympus: CameraSettings preview
    if let Some(p) = crate::vendor::orf::preview(bytes) {
        found.push(p);
    }
    let best = found
        .into_iter()
        .filter(|s| is_dct_jpeg(s) || is_jxl(s))
        .max_by_key(|s| s.len())
        .map(|s| if is_jxl(s) { s.to_vec() } else { trim_eoi(s).to_vec() });
    // a TIFF raw we can't decode whose preview no tag points to (Leaf MOS, Epson ERF)
    best.or_else(|| format.filter(|f| !f.is_supported()).and_then(|_| scan_for_jpeg(bytes)))
}

/// The end of the DCT JPEG whose SOI is at `soi` (exclusive index after its EOI) and its component count,
/// walking the marker segments and the entropy-coded data. `None` when it is not a complete, displayable
/// (baseline or progressive) JPEG, so a stray `D8 FF` in other data is rejected. `*reach` is left at
/// the furthest byte looked at (what the walk cost; see [`scan_for_jpeg`]).
/// Where a preview lives in a file, so a caller with a seekable file can read just that part of it.
///
/// Why it exists: a 56 MB Fuji RAF keeps its JPEG in the first 5.6 MB (measured: DSCF9492.RAF at
/// 5.6 MB), so a grid thumbnail or a loupe preview has no reason to pull the remaining 50 MB —
/// which on a network share is the difference between a moment and half a minute. A caller that has
/// only bytes (`wasm`, tests) keeps using [`embedded_preview`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewRange {
    /// Byte offset of the preview in the file.
    pub offset: u64,
    /// Bytes to read at [`Self::offset`]: the container's declared preview extent, which may carry
    /// padding past the preview's end-of-image marker.
    pub window_len: u64,
}

/// The preview's range in the file, when the file's header alone locates it.
///
/// `bytes` is the file's first [`PREVIEW_PREFIX`] bytes (more is fine). `None` means "read the file
/// whole and call [`embedded_preview`]": the formats not covered here (a TIFF-based raw's pointer
/// tag can sit in an IFD the prefix cuts short; CR3 scans box by box) and files whose pointer is
/// damaged.
///
/// The caller reads `[range.offset, range.offset + range.window_len)` and passes those bytes to
/// [`embedded_preview_from_range`], which returns exactly what [`embedded_preview`] returns for the
/// whole file.
///
/// Pass a prefix that actually covers the pointer's range (read [`Self::offset`] +
/// [`Self::window_len`] bytes, or start with [`PREVIEW_PREFIX`] and grow when it is not enough):
/// [`crate::vendor::raf::header`] clamps a slice to the buffer it was given, so a prefix that stops
/// inside the declared extent yields a shorter `window_len` than the file really holds.
pub fn bounded_preview(bytes: &[u8]) -> Option<PreviewRange> {
    if bytes.starts_with(b"FUJIFILMCCD-RAW") {
        let jpeg = crate::vendor::raf::header(bytes).ok()?.jpeg?;
        let offset = (jpeg.as_ptr() as usize).checked_sub(bytes.as_ptr() as usize)?;
        if offset.checked_add(jpeg.len())? > bytes.len() {
            return None;
        }
        return Some(PreviewRange { offset: offset as u64, window_len: jpeg.len() as u64 });
    }
    hasselblad_preview_range(bytes)
}

/// A Hasselblad 3FR's preview range from its header alone.
///
/// Layout (measured on the author's 907X CFV 100C files; everything below sits in the first
/// 4 KiB): IFD0 is the preview — 3888×2918, JPEG compression, one strip holding the whole
/// JPEG at the file's tail; the SubIFD is the uncompressed raw mosaic; the EXIF IFD carries
/// no preview pointers and the maker note no JPEG. The rule returns the largest single-JPEG
/// window the main chain, SubIFDs and EXIF/GPS/Interop IFDs name — the window
/// [`embedded_preview`] finds for the measured files — and `None` (read the file whole)
/// unless every IFD it would consult lies fully inside the prefix with nothing it cannot
/// mirror: a JPEG IFD that is not one strip, JPEG tiles, a half-readable pointer pair, a
/// vendor maker note (Hasselblad's is headerless and holds no preview on the measured body;
/// the whole-file search also tries it as a plain IFD, which the header rule does not), or
/// anything past the prefix at all. A body that hid a larger preview only in its maker note
/// would get the IFD0 preview from the header path; the caller still validates the window
/// as a DCT JPEG and falls back to the whole file if it is not one.
fn hasselblad_preview_range(bytes: &[u8]) -> Option<PreviewRange> {
    let order = if bytes.starts_with(b"II\x2A\x00") {
        ByteOrder::Little
    } else if bytes.starts_with(b"MM\x00\x2A") {
        ByteOrder::Big
    } else {
        return None;
    };
    let mut seen = HashSet::new();
    let mut best: Option<(u64, u64)> = None;
    let mut off = order.read_u32(bytes, 4)? as u64;
    let mut first = true;
    while off != 0 {
        off = hasselblad_ifd(bytes, order, off, first, 0, &mut seen, &mut best)?;
        first = false;
    }
    let (offset, window_len) = best?;
    (window_len > 0).then_some(PreviewRange { offset, window_len })
}

/// How deep the header walk follows child IFDs, and how many IFDs it visits in all — the
/// TIFF parser's own limits (`ParseOptions::default` in `lightcraft_tiff::reader`), so a
/// crafted prefix can neither recurse without bound nor keep the walk busy. The caller
/// passes a 4 KiB prefix, but [`bounded_preview`] accepts more.
const MAX_HEADER_IFD_DEPTH: usize = 10;
const MAX_HEADER_IFDS: usize = 1024;

/// Scan the IFD at `off`: collect its JPEG windows into `best` and follow its child IFDs.
/// Returns the next IFD's offset. `None` means "this rule cannot mirror the file":
/// read it whole instead.
fn hasselblad_ifd(
    bytes: &[u8],
    order: ByteOrder,
    off: u64,
    is_ifd0: bool,
    depth: usize,
    seen: &mut HashSet<u64>,
    best: &mut Option<(u64, u64)>,
) -> Option<u64> {
    if depth > MAX_HEADER_IFD_DEPTH || seen.len() >= MAX_HEADER_IFDS || !seen.insert(off) {
        return None;
    }
    let off = usize::try_from(off).ok()?;
    let n = order.read_u16(bytes, off)? as usize;
    let end = off.checked_add(2)?.checked_add(n.checked_mul(12)?)?.checked_add(4)?;
    if end > bytes.len() {
        return None;
    }
    let mut entries = Vec::with_capacity(n.min(64));
    for i in 0..n {
        let p = off + 2 + i * 12;
        entries.push(PrefixEntry {
            tag: order.read_u16(bytes, p)?,
            typ: order.read_u16(bytes, p + 2)?,
            count: order.read_u32(bytes, p + 4)?,
            field: p + 8,
        });
    }
    let find = |tag: u16| entries.iter().find(|e| e.tag == tag);
    // IFD0 names the make: this rule is Hasselblad 3FR only.
    if is_ifd0 {
        let make = find(t::MAKE).filter(|e| e.typ == 2).and_then(|e| entry_bytes(bytes, order, e))?;
        let make = make.split(|b| *b == 0).next().unwrap_or_default();
        if !make.eq_ignore_ascii_case(b"Hasselblad") {
            return None;
        }
    }
    let mut consider = |off: u64, len: u64| {
        if len > 0 && best.is_none_or(|(_, l)| len >= l) {
            *best = Some((off, len));
        }
    };
    // JPEGInterchangeFormat pair (either half missing means no pointer here, as in the
    // whole-file search; an unreadable half means the rule cannot mirror it)
    if let (Some(a), Some(b)) = (find(t::JPEG_INTERCHANGE_FORMAT), find(t::JPEG_INTERCHANGE_FORMAT_LENGTH)) {
        consider(single_value(bytes, order, a)?, single_value(bytes, order, b)?);
    }
    // JPEG strips: exactly one strip, mirroring the whole-file search's single chunk. Several
    // strips (which the search skips unless they merge) or JPEG tiles fall back instead.
    if let Some(e) = find(t::COMPRESSION) {
        let compression = single_value(bytes, order, e)?;
        if compression == t::compression::JPEG_XL as u64 {
            // a JPEG XL preview the range reader cannot validate: fall back (it needs the full read)
            return None;
        }
        let jpeg = [t::compression::OLD_JPEG, t::compression::JPEG, t::compression::LOSSY_JPEG].iter().any(|&c| compression == c as u64);
        if jpeg {
            match (find(t::STRIP_OFFSETS), find(t::STRIP_BYTE_COUNTS)) {
                (Some(s), Some(l)) if s.count == 1 && l.count == 1 => consider(single_value(bytes, order, s)?, single_value(bytes, order, l)?),
                (None, None) => {}
                _ => return None,
            }
            if find(t::TILE_OFFSETS).is_some() || find(t::TILE_BYTE_COUNTS).is_some() {
                return None;
            }
        }
    }
    // whole JPEG files stored as an undefined-type tag value (count and SOI gate as whole-file)
    for e in entries.iter().filter(|e| e.typ == 7 && e.count > 1024) {
        let at = order.read_u32(bytes, e.field)? as u64;
        let start = usize::try_from(at).ok()?;
        let head = bytes.get(start..start.checked_add(2)?)?;
        if head == [0xFF, 0xD8] {
            consider(at, e.count as u64);
        }
    }
    // child IFDs, as the whole-file search follows them (their next pointers ignored, as there)
    for tag in [t::SUB_IFDS, t::EXIF_IFD, t::GPS_IFD, t::INTEROP_IFD] {
        let Some(e) = find(tag) else { continue };
        for o in child_offsets(bytes, order, e)? {
            hasselblad_child(bytes, order, o, depth + 1, seen, best)?;
        }
    }
    // a vendor maker note may hold a preview IFD the prefix cannot see: fall back (the
    // whole-file search parses it for one). Headerless notes (Hasselblad's on the measured
    // body) are tried as a plain IFD there and skipped here — see the layout note above.
    if let Some(e) = find(t::MAKER_NOTE)
        && let Some((at, len)) = entry_range(bytes, order, e)
        && len > 16
        && let Ok(start) = usize::try_from(at)
        && let Some(end) = start.checked_add(16)
        && let Some(head) = bytes.get(start..end)
        && makernote::has_vendor_header(head)
    {
        return None;
    }
    if find(t::MAKER_NOTE).is_some_and(|e| vendor_note_unreadable(bytes, order, e)) {
        return None;
    }
    order.read_u32(bytes, off + 2 + n * 12).map(|v| v as u64)
}

/// Scan a child IFD (its next pointer ignored, as in the whole-file search).
fn hasselblad_child(bytes: &[u8], order: ByteOrder, off: u64, depth: usize, seen: &mut HashSet<u64>, best: &mut Option<(u64, u64)>) -> Option<()> {
    hasselblad_ifd(bytes, order, off, false, depth, seen, best).map(|_| ())
}

/// Offsets a child-IFD pointer names (single or array, SHORT/LONG/IFD): all must read cleanly.
fn child_offsets(bytes: &[u8], order: ByteOrder, e: &PrefixEntry) -> Option<Vec<u64>> {
    let raw = entry_bytes(bytes, order, e)?;
    let mut out = Vec::with_capacity((e.count as usize).min(64));
    for i in 0..e.count as usize {
        out.push(match e.typ {
            3 => order.read_u16(raw, i * 2)? as u64,
            4 | 13 => order.read_u32(raw, i * 4)? as u64,
            _ => return None,
        });
    }
    Some(out)
}

/// A SHORT or LONG single value, held in the entry itself.
fn single_value(bytes: &[u8], order: ByteOrder, e: &PrefixEntry) -> Option<u64> {
    match (e.typ, e.count) {
        (3, 1) => order.read_u16(bytes, e.field).map(u64::from),
        (4, 1) => order.read_u32(bytes, e.field).map(u64::from),
        _ => None,
    }
}

/// Byte size of a TIFF field type (classic TIFF; unassigned types bail the rule out).
fn field_size(typ: u16) -> Option<u64> {
    const SIZES: [u64; 17] = [0, 1, 1, 2, 4, 8, 1, 1, 2, 4, 8, 4, 8, 4, 0, 0, 8];
    SIZES.get(typ as usize).copied().filter(|s| *s > 0)
}

/// An entry's value bytes: inline, or at its offset when fully inside the prefix.
fn entry_bytes<'a>(bytes: &'a [u8], order: ByteOrder, e: &PrefixEntry) -> Option<&'a [u8]> {
    let total = (e.count as u64).checked_mul(field_size(e.typ)?)?;
    if total <= 4 {
        return bytes.get(e.field..e.field + 4).map(|f| &f[..total.min(4) as usize]);
    }
    let at = usize::try_from(order.read_u32(bytes, e.field)? as u64).ok()?;
    let len = usize::try_from(total).ok()?;
    bytes.get(at..at.checked_add(len)?)
}

/// An entry's value range (offset, byte length), for readers that clamp to the buffer.
fn entry_range(bytes: &[u8], order: ByteOrder, e: &PrefixEntry) -> Option<(u64, u64)> {
    let total = (e.count as u64).checked_mul(field_size(e.typ)?)?;
    if total <= 4 {
        return Some((e.field as u64, total));
    }
    Some((order.read_u32(bytes, e.field)? as u64, total))
}

/// Whether the note's vendor header cannot be ruled out: longer than 16 bytes (so it could be
/// a vendor note) without its first 16 bytes inside the prefix. Shorter notes cannot hold a
/// header plus an IFD, so the whole-file search finds nothing in them either.
fn vendor_note_unreadable(bytes: &[u8], order: ByteOrder, e: &PrefixEntry) -> bool {
    let Some((at, len)) = entry_range(bytes, order, e) else { return true };
    if len <= 16 {
        return false;
    }
    let Ok(start) = usize::try_from(at) else { return true };
    start.checked_add(16).and_then(|end| bytes.get(start..end)).is_none()
}

/// One IFD entry read from the prefix: tag, type, count, and the offset of its 4 value bytes.
struct PrefixEntry {
    tag: u16,
    typ: u16,
    count: u32,
    field: usize,
}

/// The preview inside `window`, which holds the file's bytes at `[range.offset, range.offset +
/// range.window_len)`: the counterpart of [`bounded_preview`]. Padding past the end-of-image marker
/// is dropped, exactly as [`embedded_preview`] drops it.
pub fn embedded_preview_from_range(window: &[u8], range: PreviewRange) -> Option<Vec<u8>> {
    let preview = window.get(..usize::try_from(range.window_len).ok()?)?;
    if !is_dct_jpeg(preview) {
        return None;
    }
    Some(trim_eoi(preview).to_vec())
}

/// How many bytes of a raw file are enough for [`bounded_preview`]: every covered format keeps its
/// preview pointer in a fixed header, well inside this.
pub const PREVIEW_PREFIX: usize = 4096;

fn jpeg_extent(b: &[u8], soi: usize, reach: &mut usize) -> Option<(usize, u8)> {
    let mut i = soi + 2;
    let r = walk_jpeg(b, &mut i);
    *reach = i;
    r
}

fn walk_jpeg(b: &[u8], i: &mut usize) -> Option<(usize, u8)> {
    let mut components = None;
    for _ in 0..4096 {
        if *b.get(*i)? != 0xff {
            return None;
        }
        while *b.get(*i)? == 0xff {
            *i += 1;
        }
        let m = *b.get(*i)?;
        *i += 1;
        match m {
            0xd9 => return components.map(|c| (*i, c)),
            0x01 | 0xd0..=0xd8 => continue,
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes([*b.get(*i)?, *b.get(*i + 1)?]));
        if len < 2 {
            return None;
        }
        match m {
            0xc0..=0xc2 => {
                // precision, height, width, component count
                let h = u16::from_be_bytes([*b.get(*i + 3)?, *b.get(*i + 4)?]);
                let w = u16::from_be_bytes([*b.get(*i + 5)?, *b.get(*i + 6)?]);
                if h == 0 || w == 0 || components.is_some() {
                    return None;
                }
                components = Some(*b.get(*i + 7)?);
            }
            0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => return None, // lossless, arithmetic, hierarchical
            _ => {}
        }
        *i += len;
        if m == 0xda {
            // entropy-coded data up to the next marker that is not a stuffed 0xff or a restart
            loop {
                *i += b.get(*i..)?.iter().position(|&x| x == 0xff)?;
                match *b.get(*i + 1)? {
                    0x00 | 0xd0..=0xd7 => *i += 2,
                    0xff => *i += 1,
                    _ => break,
                }
            }
        }
    }
    None
}

/// Last resort for the containers whose structure is not walked (Canon CRW, Minolta MRW, Sigma X3F, TIFF raws
/// with private blocks such as Leaf MOS or Epson ERF): the largest complete colour DCT JPEG stored as a plain
/// byte run in the file. A one-component JPEG is skipped (a Canon PowerShot CRW stores its sensor mosaic as one).
///
/// Evidence: in the Minolta and Epson files the first byte of the stored preview is not `ff` (it is `00`, `02` or
/// `ee`) while the rest is a regular JPEG, so a run is found by its `d8 ff` and starts one byte earlier,
/// with that byte restored to `ff`.
///
/// A crafted file can hold millions of JPEG-like starts whose walks each run to its end: the walks
/// share a budget of eight times the file's size, so the scan stays linear (a real file's candidates
/// fail within a few bytes, or are the preview itself).
fn scan_for_jpeg(data: &[u8]) -> Option<Vec<u8>> {
    let mut best: Option<(usize, usize)> = None;
    let mut at = 1;
    let mut budget = data.len().saturating_mul(8).max(1 << 20);
    while let Some(off) = data.get(at..).and_then(|s| s.iter().position(|&b| b == 0xd8)) {
        let d8 = at + off;
        at = d8 + 1;
        if data.get(d8 + 1) != Some(&0xff) || !matches!(data.get(d8 + 2), Some(0xc0..=0xc4 | 0xdb | 0xe0..=0xef | 0xfe)) {
            continue;
        }
        let start = d8 - 1; // `at` starts at 1, so d8 >= 1
        let mut reach = start;
        let found = jpeg_extent(data, start, &mut reach);
        budget = budget.saturating_sub(reach.saturating_sub(start));
        if budget == 0 {
            break;
        }
        if let Some((end, 3)) = found {
            if best.is_none_or(|(s, e)| end - start > e - s) {
                best = Some((start, end));
            }
            at = end;
        }
    }
    let (start, end) = best?;
    let mut jpeg = data.get(start..end)?.to_vec();
    if let Some(first) = jpeg.first_mut() {
        *first = 0xff;
    }
    Some(jpeg)
}

/// Canon CR3: the full-size JPEG track (see [`lightcraft_meta::cr3`]), else the `PRVW` / `THMB` boxes.
fn cr3_preview(bytes: &[u8]) -> Option<&[u8]> {
    let full = lightcraft_meta::cr3::parse_cr3(bytes)
        .and_then(|c| c.tracks.iter().find(|t| t.kind == lightcraft_meta::cr3::Cr3TrackKind::Jpeg).and_then(|t| t.data));
    if let Some(j) = full.and_then(|(at, len)| bytes.get(at..at.checked_add(len)?)).filter(|j| is_dct_jpeg(j)) {
        return Some(j);
    }
    cr3_preview_boxes(bytes)
}

/// Canon CR3 (ISO base media file): the `PRVW` box (Laurent Clévy's CR3 notes; layout confirmed on a CC0
/// sample) is `u32 size, "PRVW", u32 0, u16 ?, u16 width, u16 height, u16 ?, u32 jpeg length, JPEG`; the smaller
/// `THMB` box has the same shape. Returns the larger valid one.
fn cr3_preview_boxes(bytes: &[u8]) -> Option<&[u8]> {
    let mut best: Option<&[u8]> = None;
    for tag in [b"PRVW", b"THMB"] {
        let mut from = 0;
        while let Some(i) = bytes.get(from..).and_then(|s| s.windows(4).position(|w| w == tag)).map(|p| p + from) {
            from = i + 4;
            let Some(start) = i.checked_sub(4) else { continue };
            let be32 = |at: usize| bytes.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize);
            let (Some(size), Some(len)) = (be32(start), be32(start + 20)) else { continue };
            let j = start + 24;
            if len < 4 || j + len > start + size.max(24) || j + len > bytes.len() {
                continue;
            }
            let jpeg = &bytes[j..j + len];
            if is_dct_jpeg(jpeg) {
                if best.is_none_or(|b| b.len() < jpeg.len()) {
                    best = Some(jpeg);
                }
                break;
            }
        }
    }
    best
}

/// Trim trailing garbage after the last EOI when a stored length over-reports.
fn trim_eoi(s: &[u8]) -> &[u8] {
    let end = s.windows(2).rposition(|w| w == [0xff, 0xd9]).map(|p| p + 2).unwrap_or(s.len());
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_tiff::{IfdBuilder, ImageData, TiffWriter, Value};

    fn fake_jpeg(n: usize) -> Vec<u8> {
        let mut j = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 8, 0, 1, 0, 1, 1, 1, 0x11, 0];
        j.extend(std::iter::repeat_n(0x55u8, n));
        j.extend_from_slice(&[0xff, 0xd9]);
        j
    }

    /// A minimal RAF: the fixed header (JPEG offset/length at 84/88), the JPEG, then a raw tail the
    /// caller must not have to read. `jpeg_len` is written as declared, so a test can claim more than
    /// the JPEG holds the way a padded container does.
    fn raf_fixture(jpeg: &[u8], gap: usize, preamble: usize, declared_len: Option<usize>) -> Vec<u8> {
        let head = 160 + preamble;
        let jpeg_off = head;
        let mut f = vec![0u8; head];
        f[..16].copy_from_slice(b"FUJIFILMCCD-RAW ");
        f[84..88].copy_from_slice(&(jpeg_off as u32).to_be_bytes());
        f[88..92].copy_from_slice(&(declared_len.unwrap_or(jpeg.len()) as u32).to_be_bytes());
        // records directory and raw block are absent (offset 0), which `header` reads as "none"
        f.extend_from_slice(jpeg);
        f.extend(std::iter::repeat_n(0x33u8, gap));
        f
    }

    /// A minimal Hasselblad-shaped TIFF: IFD0 is a single-JPEG-strip preview (like a 3FR's),
    /// the SubIFD an uncompressed mosaic, EXIF a headerless note. The JPEG sits past 4096 so a
    /// test can prove the header alone locates it. `make` names the maker; `note_head` starts
    /// the note (which holds no JPEG); `strips` sets IFD0's strip counts. (Twin of the engine
    /// tests' `tiff_3fr_with_embedded_jpeg`, which feeds the loader instead; keep the layouts
    /// in step.)
    fn hasselblad_fixture(jpeg: &[u8], make: &[u8], note_head: &[u8], strips: u32) -> Vec<u8> {
        hasselblad_fixture_order(jpeg, make, note_head, strips, ByteOrder::Little)
    }

    fn hasselblad_fixture_order(jpeg: &[u8], make: &[u8], note_head: &[u8], strips: u32, order: ByteOrder) -> Vec<u8> {
        let w16 = |v: u16| match order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        };
        let w32 = |v: u32| match order {
            ByteOrder::Little => v.to_le_bytes(),
            ByteOrder::Big => v.to_be_bytes(),
        };
        let entry = |tag: u16, typ: u16, cnt: u32, val: u32| {
            let mut e = Vec::with_capacity(12);
            e.extend_from_slice(&w16(tag));
            e.extend_from_slice(&w16(typ));
            e.extend_from_slice(&w32(cnt));
            // a single inline SHORT sits in the field's first two bytes (either order)
            if typ == 3 && cnt == 1 {
                e.extend_from_slice(&w16(val as u16));
                e.extend_from_slice(&[0, 0]);
            } else {
                e.extend_from_slice(&w32(val));
            }
            e
        };
        let mut f = vec![0u8; 8];
        f[..4].copy_from_slice(match order {
            ByteOrder::Little => b"II\x2A\x00",
            ByteOrder::Big => b"MM\x00\x2A",
        });
        f[4..8].copy_from_slice(&w32(8));
        let table0 = 8 + 2 + 8 * 12 + 4; // 110
        let make_at = table0 as u32;
        let sub_at = make_at + make.len() as u32;
        let exif_at = sub_at + (2 + 5 * 12 + 4) as u32;
        let note_at = exif_at + (2 + 12 + 4) as u32;
        let note_len = 2048u32;
        let jpeg_off = 6000u32;
        let mut ifd0 = w16(8).to_vec();
        for e in [
            entry(0x0100, 3, 1, 3888),
            entry(0x0101, 3, 1, 2918),
            entry(0x0103, 3, 1, 7),
            entry(0x010F, 2, make.len() as u32, make_at),
            entry(0x0111, 4, strips, if strips == 1 { jpeg_off } else { 0 }),
            entry(0x0117, 4, strips, if strips == 1 { jpeg.len() as u32 } else { 0 }),
            entry(0x014A, 4, 1, sub_at),
            entry(0x8769, 4, 1, exif_at),
        ] {
            ifd0.extend(e);
        }
        ifd0.extend(w32(0));
        f.extend(ifd0);
        f.extend_from_slice(make);
        let mut sub = w16(5).to_vec();
        for e in
            [entry(0x0100, 3, 1, 11904), entry(0x0101, 3, 1, 8842), entry(0x0103, 3, 1, 1), entry(0x0111, 4, 1, 16384), entry(0x0117, 4, 1, 1000)]
        {
            sub.extend(e);
        }
        sub.extend(w32(0));
        f.extend(sub);
        let mut exif = w16(1).to_vec();
        exif.extend(entry(0x927C, 7, note_len, note_at));
        exif.extend(w32(0));
        f.extend(exif);
        let mut note = vec![0x23u8; note_len as usize];
        note[..note_head.len()].copy_from_slice(note_head);
        f.extend(note);
        assert!(f.len() < jpeg_off as usize, "the header stays inside the prefix");
        f.resize(jpeg_off as usize, 0x33);
        f.extend_from_slice(jpeg);
        f.extend(std::iter::repeat_n(0x44u8, 10_000));
        f
    }

    /// A 3FR's preview pointer sits in IFD0, so a header-sized prefix locates it and the caller
    /// reads just that range instead of the whole 200 MB file.
    #[test]
    fn bounded_preview_locates_a_hasselblads_jpeg_from_the_header_alone() {
        let jpeg = fake_jpeg(5000);
        let file = hasselblad_fixture(&jpeg, b"Hasselblad\0", b"#\x00", 1);
        let prefix = &file[..4096];
        let range = bounded_preview(prefix).expect("a 3FR's preview is found from its header");
        assert_eq!(range.offset, 6000);
        assert_eq!(range.window_len, jpeg.len() as u64);
        assert!((range.offset + range.window_len) < file.len() as u64, "the range stops short of the tail: {range:?} of {}", file.len());
        // what the caller reads, and what it holds afterwards: identical to the whole-file answer
        let window = &file[range.offset as usize..(range.offset + range.window_len) as usize];
        assert_eq!(embedded_preview_from_range(window, range).as_deref(), embedded_preview(&file).as_deref());
    }

    /// The header rule reads a big-endian TIFF header too (Hasselblad writes little-endian,
    /// but the walker is order-generic and the inline SHORT values sit differently).
    #[test]
    fn bounded_preview_locates_a_hasselblads_jpeg_from_a_big_endian_header() {
        let jpeg = fake_jpeg(5000);
        let file = hasselblad_fixture_order(&jpeg, b"Hasselblad\0", b"#\x00", 1, ByteOrder::Big);
        let range = bounded_preview(&file[..4096]).expect("a big-endian 3FR header locates its preview");
        assert_eq!(range.offset, 6000);
        assert_eq!(range.window_len, jpeg.len() as u64);
        let window = &file[range.offset as usize..(range.offset + range.window_len) as usize];
        assert_eq!(embedded_preview_from_range(window, range).as_deref(), embedded_preview(&file).as_deref());
    }

    /// A crafted header cannot keep the walk busy: a child IFD pointing back at an IFD
    /// already visited bails the rule out, and so does nesting past the depth cap.
    #[test]
    fn bounded_preview_bails_on_cyclic_and_deep_ifd_chains() {
        let jpeg = fake_jpeg(5000);
        let mut cyclic = hasselblad_fixture(&jpeg, b"Hasselblad\0", b"#\x00", 1);
        // IFD0 sits at 8 (count, then 12-byte entries); its SubIFDs entry is the seventh,
        // whose value field is at 8 + 2 + 6 * 12 + 8 = 90. Point it back at IFD0.
        cyclic[90..94].copy_from_slice(&8u32.to_le_bytes());
        assert!(bounded_preview(&cyclic[..4096]).is_none(), "a cyclic SubIFD falls back");
        // the depth cap itself: the same IFD0 scans at depth 0 and bails past the cap
        let mut seen = HashSet::new();
        let mut best = None;
        assert!(hasselblad_ifd(&cyclic[..4096], ByteOrder::Little, 8, true, MAX_HEADER_IFD_DEPTH + 1, &mut seen, &mut best).is_none());
    }

    /// Anything the header rule cannot mirror falls back to the whole-file search:
    /// another maker, a cut table, several strips, a vendor note.
    #[test]
    fn bounded_preview_bails_unless_the_whole_layout_is_visible() {
        let jpeg = fake_jpeg(5000);
        let file = hasselblad_fixture(&jpeg, b"Hasselblad\0", b"#\x00", 1);
        assert!(bounded_preview(&file[..100]).is_none(), "a cut IFD0 falls back");
        assert!(bounded_preview(&hasselblad_fixture(&jpeg, b"Canon\0", b"#\x00", 1)[..4096]).is_none(), "another maker falls back");
        assert!(bounded_preview(&hasselblad_fixture(&jpeg, b"Hasselblad\0", b"#\x00", 2)[..4096]).is_none(), "several strips fall back");
        assert!(
            bounded_preview(&hasselblad_fixture(&jpeg, b"Hasselblad\0", b"Nikon\0\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00", 1)[..4096])
                .is_none(),
            "a vendor note falls back"
        );
        assert!(bounded_preview(&hasselblad_fixture(&jpeg, b"\0", b"#\x00", 1)[..4096]).is_none(), "a missing make falls back");
    }

    /// A RAF's preview pointer sits at a fixed offset, so a header-sized prefix locates it and the
    /// caller reads just that range instead of the whole file — the whole point on a network share.
    #[test]
    fn bounded_preview_locates_a_rafs_jpeg_from_the_header_alone() {
        let jpeg = fake_jpeg(5000);
        let file = raf_fixture(&jpeg, 40_000, 0, None);
        // A prefix that covers the pointer's range: header + JPEG, and nothing of the tail.
        let prefix_len = 8192.min(file.len());
        let prefix = &file[..prefix_len];

        let range = bounded_preview(prefix).expect("a RAF's preview is found from its header");
        assert_eq!(range.offset, 160);
        assert_eq!(range.window_len, jpeg.len() as u64);
        assert!(range.offset + range.window_len <= prefix_len as u64, "the prefix covers the whole range: {range:?} of {prefix_len}");
        assert!((range.offset + range.window_len) < file.len() as u64, "the range stops short of the tail: {range:?} of {}", file.len());

        // What the caller reads, and what it holds afterwards: identical to the whole-file answer.
        let window = prefix.get(range.offset as usize..(range.offset + range.window_len) as usize).expect("the range lies inside the prefix");
        let bounded = embedded_preview_from_range(window, range).expect("the range holds the preview");
        assert_eq!(Some(bounded.clone()), embedded_preview(&file));
        assert!(bounded.ends_with(&[0xff, 0xd9]), "the preview ends at its EOI marker");

        // A prefix that stops inside the declared extent is clamped by the container reader. The
        // caller then reads a short window; what comes back is the whole JPEG it holds (the EOI
        // marker is inside), never bytes past the window.
        let short = &file[..2000];
        let clipped = bounded_preview(short).expect("a pointer before the cut is still found");
        assert!(clipped.window_len < jpeg.len() as u64, "clamped: {clipped:?}");
        let window = short.get(clipped.offset as usize..).expect("inside");
        let preview = embedded_preview_from_range(window, clipped).expect("the bytes are a JPEG start");
        // No end-of-image marker inside the window: the bytes come back as they are, exactly as the
        // whole-file path returns an unclosed JPEG. The decoder refuses it later; nothing is invented.
        assert_eq!(preview, window, "an unclosed JPEG is handed on unchanged, never padded");
        assert!(preview.len() <= window.len(), "never past the window");
    }

    /// The preview pointer may sit past the prefix (a long maker-note record before it), and a
    /// container may declare padding after the JPEG: both stay correct, and the padding is dropped.
    #[test]
    fn bounded_preview_handles_a_late_pointer_and_padding() {
        let jpeg = fake_jpeg(300);
        let late = raf_fixture(&jpeg, 0, 9000, None);
        let range = bounded_preview(&late).expect("the whole file is always a valid prefix");
        assert_eq!(range.offset, (160 + 9000) as u64);
        let padded = raf_fixture(&jpeg, 500, 0, Some(jpeg.len() + 500));
        let range = bounded_preview(&padded).expect("a padded extent is still located");
        assert_eq!(range.window_len, (jpeg.len() + 500) as u64);
        let window = padded.get(range.offset as usize..(range.offset + range.window_len) as usize).expect("the window is inside the file");
        let preview = embedded_preview_from_range(window, range).expect("the padding is dropped");
        assert_eq!(preview, embedded_preview(&padded).expect("the whole-file answer agrees"));
        assert!(preview.ends_with(&[0xff, 0xd9]));
    }

    /// Without a usable header or pointer the caller must read the file whole: the answer is `None`,
    /// never a wrong range.
    #[test]
    fn bounded_preview_declines_what_the_header_cannot_answer() {
        let jpeg = fake_jpeg(64);
        let file = raf_fixture(&jpeg, 0, 0, None);
        assert_eq!(bounded_preview(&jpeg), None, "not a raw at all");
        assert_eq!(bounded_preview(b""), None);
        if let Some(short) = file.get(..64) {
            assert_eq!(bounded_preview(short), None, "a header cut short has not shown the pointer yet");
        }
        // a pointer that claims more than the file holds: `header` clamps the slice to the buffer,
        // so the range never runs past the end (no out-of-bounds read can follow from it)
        let mut lying = raf_fixture(&jpeg, 4096, 0, None);
        lying[88..92].copy_from_slice(&(u32::MAX / 2).to_be_bytes());
        let clamped = bounded_preview(&lying).expect("a pointer inside the file is still usable");
        assert!(clamped.offset + clamped.window_len <= lying.len() as u64, "clamped to the file: {clamped:?}");
        // a range the window cannot satisfy yields nothing
        assert_eq!(embedded_preview_from_range(&jpeg, PreviewRange { offset: 0, window_len: u64::MAX }), None);
        // a TIFF-based raw: its pointer tag lives in an IFD this prefix may cut short
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&[0u8; 64]);
        assert_eq!(bounded_preview(&tiff), None);
    }

    /// A range that does not hold a JPEG yields nothing, rather than a truncated image handed on.
    #[test]
    fn a_range_that_does_not_hold_a_jpeg_yields_nothing() {
        let jpeg = fake_jpeg(200);
        let range = PreviewRange { offset: 0, window_len: jpeg.len() as u64 };
        assert_eq!(embedded_preview_from_range(&jpeg, range).as_deref(), Some(&jpeg[..]));
        assert_eq!(embedded_preview_from_range(&jpeg[..10], range), None, "window shorter than the range claims");
        let not_jpeg = vec![0u8; jpeg.len()];
        assert_eq!(embedded_preview_from_range(&not_jpeg, range), None, "not a JPEG");
        assert_eq!(embedded_preview_from_range(&[], PreviewRange { offset: 0, window_len: 0 }), None, "empty window");
    }

    #[test]
    fn picks_largest_dct_jpeg() {
        let small = fake_jpeg(10);
        let big = fake_jpeg(500);
        let lossless = crate::ljpeg::encode(&[1u16; 64], 8, 8, 1, 12, 1, 0);
        let mut ifd0 = IfdBuilder::new();
        ifd0.set(t::COMPRESSION, Value::Short(vec![6]));
        ifd0.set(t::IMAGE_WIDTH, Value::Long(vec![1]));
        ifd0.set(t::IMAGE_LENGTH, Value::Long(vec![1]));
        ifd0.set_image(ImageData::Strips { rows_per_strip: 1, strips: vec![small.clone()] });
        let mut raw = IfdBuilder::new();
        raw.set(t::COMPRESSION, Value::Short(vec![7]));
        raw.set(t::IMAGE_WIDTH, Value::Long(vec![8]));
        raw.set(t::IMAGE_LENGTH, Value::Long(vec![8]));
        raw.set_image(ImageData::Strips { rows_per_strip: 8, strips: vec![lossless] });
        ifd0.add_sub_ifd(raw);
        let mut sub = IfdBuilder::new();
        sub.set(t::COMPRESSION, Value::Short(vec![7]));
        sub.set(t::IMAGE_WIDTH, Value::Long(vec![2]));
        sub.set(t::IMAGE_LENGTH, Value::Long(vec![2]));
        sub.set(t::NEW_SUBFILE_TYPE, Value::Long(vec![1]));
        sub.set_image(ImageData::Strips { rows_per_strip: 2, strips: vec![big.clone()] });
        ifd0.add_sub_ifd(sub);
        let bytes = TiffWriter::default().write(&[ifd0]).unwrap();
        assert_eq!(embedded_preview(&bytes).unwrap(), big);
        assert!(embedded_preview(b"nope").is_none());
        // CR3: a PRVW box after the ftyp
        let mut cr3 = b"\0\0\0\x18ftypcrx \0\0\0\x01crx isom".to_vec();
        let j = fake_jpeg(300);
        cr3.extend_from_slice(&((24 + j.len()) as u32).to_be_bytes());
        cr3.extend_from_slice(b"PRVW\0\0\0\0\0\x01\x06\x54\x04\x38\0\x01");
        cr3.extend_from_slice(&(j.len() as u32).to_be_bytes());
        cr3.extend_from_slice(&j);
        assert_eq!(embedded_preview(&cr3).unwrap(), j);
        for n in 0..cr3.len() {
            let _ = embedded_preview(&cr3[..n]);
        }
    }

    /// CR3 with a full-size JPEG track: that JPEG wins over the smaller `PRVW` box.
    #[test]
    fn cr3_prefers_the_full_size_jpeg_track() {
        let bx = |kind: &[u8; 4], body: &[u8]| -> Vec<u8> { [&((body.len() + 8) as u32).to_be_bytes()[..], kind, body].concat() };
        let full = |kind: &[u8; 4], body: &[u8]| bx(kind, &[&[0u8; 4][..], body].concat());
        let (small, big) = (fake_jpeg(300), fake_jpeg(3000));
        let mut file = bx(b"ftyp", b"crx \0\0\0\x01crx isom");
        let mut prvw = b"\0\0\0\0\0\x01\x06\x54\x04\x38\0\x01".to_vec();
        prvw.extend_from_slice(&(small.len() as u32).to_be_bytes());
        prvw.extend_from_slice(&small);
        file.extend(bx(b"PRVW", &prvw));
        // CRAW sample entry: 82 bytes, then a JPEG child box; one sample at `at`
        let mut craw = vec![0u8; 82];
        craw.extend(bx(b"JPEG", &[0; 4]));
        let at = 2048u64;
        let stsd = full(b"stsd", &[&1u32.to_be_bytes()[..], &bx(b"CRAW", &craw)].concat());
        let stsz = full(b"stsz", &[0u32.to_be_bytes(), 1u32.to_be_bytes(), (big.len() as u32).to_be_bytes()].concat());
        let co64 = full(b"co64", &[&1u32.to_be_bytes()[..], &at.to_be_bytes()].concat());
        let trak = bx(b"trak", &bx(b"mdia", &bx(b"minf", &bx(b"stbl", &[stsd, stsz, co64].concat()))));
        file.extend(bx(b"moov", &trak));
        file.resize(at as usize, 0);
        file.extend_from_slice(&big);
        assert_eq!(embedded_preview(&file).unwrap(), big);
        // a track pointing past the end falls back to PRVW
        file.truncate(at as usize + 100);
        assert_eq!(embedded_preview(&file).unwrap(), small);
    }

    /// A structurally complete DCT JPEG (no real image data): `nc` components, `n` entropy bytes.
    fn scan_jpeg(nc: u8, n: usize) -> Vec<u8> {
        let mut j = vec![0xff, 0xd8, 0xff, 0xdb, 0x00, 0x43, 0x00];
        j.extend(std::iter::repeat_n(8u8, 64));
        j.extend_from_slice(&[0xff, 0xc0, 0x00, 8 + 3 * nc, 8, 0, 16, 0, 24, nc]);
        for c in 0..nc {
            j.extend_from_slice(&[c + 1, 0x11, 0]);
        }
        j.extend_from_slice(&[0xff, 0xda, 0x00, 6 + 2 * nc, nc]);
        for c in 0..nc {
            j.extend_from_slice(&[c + 1, 0]);
        }
        j.extend_from_slice(&[0, 63, 0]);
        j.extend(std::iter::repeat_n(0x55u8, n));
        j.extend_from_slice(&[0xff, 0x00, 0x12, 0xff, 0xd0, 0x34, 0xff, 0xd9]);
        j
    }

    /// A crafted container full of JPEG-like starts (each walk running on through the ones after it)
    /// is scanned in linear time: the walks share a budget (without it this 40 MB file takes about a minute).
    #[test]
    fn a_scan_through_many_fake_jpegs_stays_linear() {
        let mut unit = vec![0u8, 0xd8, 0xff, 0xdb, 0x00, 0x02, 0xff, 0xda, 0x00, 0x02];
        unit.extend(std::iter::repeat_n(0x55u8, 190));
        let mut f = b"\0MRM\0\x01\0\0".to_vec();
        for _ in 0..200_000 {
            f.extend_from_slice(&unit);
        }
        let t = std::time::Instant::now();
        assert_eq!(scan_for_jpeg(&f), None);
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "{:?}", t.elapsed());
    }

    /// Containers whose structure we do not walk give up their largest colour JPEG, found by scanning.
    #[test]
    fn scanned_containers_give_the_largest_colour_jpeg() {
        let (small, big, mosaic) = (scan_jpeg(3, 100), scan_jpeg(3, 900), scan_jpeg(1, 5000));
        let heads: [&[u8]; 3] = [b"II\x1a\0\0\0HEAPCCDR", b"\0MRM\0\x01\0\0", b"FOVb\x02\0\x02\0"];
        for head in heads {
            let mut f = head.to_vec();
            for part in [&small, &mosaic, &big] {
                f.extend_from_slice(&[0xff, 0xd8, 0xff, 0x00, 0x00]); // a stray SOI-like run: not a JPEG
                f.extend_from_slice(part);
                f.extend_from_slice(&[0u8; 7]);
            }
            assert!(crate::probe(&f).is_some_and(|p| !p.is_supported()), "{head:?}");
            assert_eq!(embedded_preview(&f).as_deref(), Some(&big[..]), "{head:?}");
            for n in (0..f.len()).step_by(11) {
                let _ = embedded_preview(&f[..n]);
            }
        }
        // a preview whose first byte was overwritten (Minolta and Epson files store 00, 02 or ee there) is restored
        let mut f = b"\0MRM\0\x01\0\0".to_vec();
        f.push(0x02);
        f.extend_from_slice(&big[1..]);
        assert_eq!(embedded_preview(&f).as_deref(), Some(&big[..]));
        // only a one-component JPEG, or none: nothing
        let mut f = b"FOVb\x02\0\x02\0".to_vec();
        f.extend_from_slice(&mosaic);
        assert_eq!(embedded_preview(&f), None);
    }
}
