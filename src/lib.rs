use js_sys::{Array, Object, Reflect, Uint8Array};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use wasm_bindgen::prelude::*;

// A single embedded file or the complete solid stream may exceed the crate's
// conservative 64 MiB default. Keep a finite cap to avoid unbounded expansion.
const DEFAULT_MAX_DECOMPRESSED_SIZE: usize = 1024 * 1024 * 1024;
const MAX_DECOMPRESSED_SIZE_CAP: u64 = 3 * 1024 * 1024 * 1024;
const MIN_MAX_DECOMPRESSED_SIZE: usize = 16 * 1024 * 1024;
static MAX_DECOMPRESSED_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_DECOMPRESSED_SIZE);

fn max_decompressed_size() -> usize {
    MAX_DECOMPRESSED_SIZE.load(Ordering::Relaxed)
}

/// Set the maximum total decompressed/extracted output size for the next extraction.
#[wasm_bindgen]
pub fn set_extraction_limit(bytes: f64) {
    let requested = if bytes.is_finite() && bytes > 0.0 {
        bytes as u64
    } else {
        DEFAULT_MAX_DECOMPRESSED_SIZE as u64
    };
    // WASM32 cannot address a single output allocation larger than usize::MAX;
    // cap the configured value to the platform's representable size.
    let max_bytes = requested
        .clamp(MIN_MAX_DECOMPRESSED_SIZE as u64, MAX_DECOMPRESSED_SIZE_CAP)
        .min(usize::MAX as u64) as usize;
    MAX_DECOMPRESSED_SIZE.store(max_bytes, Ordering::Relaxed);
}

fn make_record(name: &str, data: &[u8]) -> Result<JsValue, JsValue> {
    let record = Object::new();
    Reflect::set(&record, &"name".into(), &JsValue::from_str(name))?;
    Reflect::set(
        &record,
        &"size".into(),
        &JsValue::from_f64(data.len() as f64),
    )?;
    Reflect::set(&record, &"data".into(), &Uint8Array::from(data))?;
    Ok(record.into())
}

fn pdf_text(object: &lopdf::Object) -> String {
    lopdf::decode_text_string(object).unwrap_or_else(|_| {
        String::from_utf8_lossy(object.as_str().unwrap_or_default()).into_owned()
    })
}

fn png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    for byte in kind.iter().chain(data) {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    output.extend_from_slice(&(!crc).to_be_bytes());
}

fn pdf_pixels_to_png(
    width: usize,
    height: usize,
    color_type: u8,
    pixels: &[u8],
) -> Option<Vec<u8>> {
    use flate2::{Compression, write::ZlibEncoder};

    let channels = match color_type {
        0 => 1,
        2 => 3,
        _ => return None,
    };
    let row_bytes = width.checked_mul(channels)?;
    if pixels.len() != row_bytes.checked_mul(height)? {
        return None;
    }
    let mut scanlines = Vec::with_capacity(pixels.len().checked_add(height)?);
    for row in pixels.chunks_exact(row_bytes) {
        scanlines.push(0); // PNG filter: None
        scanlines.extend_from_slice(row);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&scanlines).ok()?;
    let compressed = encoder.finish().ok()?;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, color_type, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &header);
    png_chunk(&mut png, b"IDAT", &compressed);
    png_chunk(&mut png, b"IEND", &[]);
    Some(png)
}

fn pdf_apply_decode(
    image: &lopdf::xobject::PdfImage<'_>,
    channels: usize,
    pixels: &mut [u8],
) -> bool {
    let ranges = match image.origin_dict.get(b"Decode") {
        Ok(lopdf::Object::Array(values)) if values.len() >= channels * 2 => values,
        Ok(_) => return false,
        Err(_) => return true,
    };
    let mut pairs = Vec::with_capacity(channels);
    for channel in 0..channels {
        let Some(min) = ranges[channel * 2].as_float().ok() else {
            return false;
        };
        let Some(max) = ranges[channel * 2 + 1].as_float().ok() else {
            return false;
        };
        pairs.push((min, max));
    }
    for sample in pixels.chunks_exact_mut(channels) {
        for (channel, value) in sample.iter_mut().enumerate() {
            let (min, max) = pairs[channel];
            let decoded = min + (*value as f32 / 255.0) * (max - min);
            *value = (decoded.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    true
}

fn pdf_collect_file_spec(
    document: &lopdf::Document,
    object: &lopdf::Object,
    suggested_name: Option<String>,
    seen_streams: &mut HashSet<lopdf::ObjectId>,
    files: &Array,
    total_bytes: &mut usize,
) -> Result<(), JsValue> {
    let (_, object) = document.dereference(object).map_err(|error| {
        JsValue::from_str(&format!("Could not read PDF attachment reference: {error}"))
    })?;
    let Ok(file_spec) = object.as_dict() else {
        return Ok(());
    };
    let embedded = match file_spec.get_deref(b"EF", document) {
        Ok(object) => object,
        Err(_) => return Ok(()),
    };
    let Ok(embedded) = embedded.as_dict() else {
        return Ok(());
    };

    for key in [b"UF".as_slice(), b"F".as_slice()] {
        let Ok(stream_ref) = embedded.get(key) else {
            continue;
        };
        let (stream_id, stream_object) = document.dereference(stream_ref).map_err(|error| {
            JsValue::from_str(&format!("Could not read PDF attachment stream: {error}"))
        })?;
        if let Some(id) = stream_id {
            if !seen_streams.insert(id) {
                continue;
            }
        }
        let stream = stream_object
            .as_stream()
            .map_err(|_| JsValue::from_str("PDF attachment does not point to a data stream."))?;
        let remaining = max_decompressed_size().saturating_sub(*total_bytes);
        let data = stream
            .get_plain_content_with_limit(remaining)
            .map_err(|error| {
                JsValue::from_str(&format!("Could not decompress PDF attachment: {error}"))
            })?;
        *total_bytes = total_bytes
            .checked_add(data.len())
            .ok_or_else(|| JsValue::from_str("PDF attachments exceed the 1 GiB browser limit."))?;
        if *total_bytes > max_decompressed_size() {
            return Err(JsValue::from_str(
                "PDF attachments exceed the 1 GiB browser limit.",
            ));
        }

        let filename = suggested_name
            .clone()
            .or_else(|| {
                [b"UF".as_slice(), b"F".as_slice()]
                    .iter()
                    .find_map(|key| file_spec.get(key).ok().map(pdf_text))
            })
            .unwrap_or_else(|| "attachment".to_owned());
        files.push(&make_record(&filename, &data)?);
        // `/UF` and `/F` often reference the same stream; one record is enough.
        if key == b"UF" {
            break;
        }
    }
    Ok(())
}

/// Extract embedded file attachments from a PDF. Text and page images are not extracted.
#[wasm_bindgen]
pub fn extract_pdf(input: &[u8]) -> Result<Array, JsValue> {
    use lopdf::Object;

    let started = js_sys::Date::now();
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[pdf] Parsing PDF ({:.1} MiB).",
        input.len() as f64 / 1_048_576.0
    )));
    let options = lopdf::LoadOptions::with_max_decompressed_size(max_decompressed_size());
    let document = lopdf::Document::load_mem_with_options(input, options)
        .map_err(|error| JsValue::from_str(&format!("Could not read PDF: {error}")))?;
    let catalog = document
        .catalog()
        .map_err(|error| JsValue::from_str(&format!("Could not read PDF catalog: {error}")))?;
    let files = Array::new();
    let mut total_bytes = 0usize;
    let mut seen_streams = HashSet::new();

    // Traverse the EmbeddedFiles name tree. `/Names` stores alternating filename
    // and file specification values; `/Kids` links to further name tree nodes.
    if let Ok(names) = catalog.get_deref(b"Names", &document) {
        if let Ok(names) = names.as_dict() {
            if let Ok(root) = names.get(b"EmbeddedFiles") {
                let mut pending = vec![root];
                let mut visited_nodes = HashSet::new();
                while let Some(node_ref) = pending.pop() {
                    let (node_id, node) = document.dereference(node_ref).map_err(|error| {
                        JsValue::from_str(&format!(
                            "Could not read PDF attachment name tree: {error}"
                        ))
                    })?;
                    if let Some(id) = node_id {
                        if !visited_nodes.insert(id) {
                            continue;
                        }
                    }
                    let Ok(dict) = node.as_dict() else {
                        continue;
                    };
                    if let Ok(entries) = dict
                        .get_deref(b"Names", &document)
                        .and_then(Object::as_array)
                    {
                        for pair in entries.chunks_exact(2) {
                            let name = Some(pdf_text(&pair[0]));
                            pdf_collect_file_spec(
                                &document,
                                &pair[1],
                                name,
                                &mut seen_streams,
                                &files,
                                &mut total_bytes,
                            )?;
                        }
                    }
                    if let Ok(kids) = dict
                        .get_deref(b"Kids", &document)
                        .and_then(Object::as_array)
                    {
                        pending.extend(kids.iter());
                    }
                }
            }
        }
    }

    // Associated files are also valid PDF attachments and may not appear in the name tree.
    if let Ok(associated) = catalog
        .get_deref(b"AF", &document)
        .and_then(Object::as_array)
    {
        for file_spec in associated {
            pdf_collect_file_spec(
                &document,
                file_spec,
                None,
                &mut seen_streams,
                &files,
                &mut total_bytes,
            )?;
        }
    }

    // Recover raster image XObjects used by pages. JPEG streams can be saved
    // as-is; Flate encoded gray/RGB pixels are wrapped in a small PNG encoder.
    let mut seen_images = HashSet::new();
    for (page_number, page_id) in document.get_pages() {
        let page_images = match document.get_page_images(page_id) {
            Ok(images) => images,
            Err(error) => {
                web_sys::console::warn_1(&JsValue::from_str(&format!(
                    "[pdf] Skipping images on page {page_number}: {error}"
                )));
                continue;
            }
        };
        for (image_number, image) in page_images.into_iter().enumerate() {
            if !seen_images.insert(image.id) {
                continue;
            }
            let width = usize::try_from(image.width).ok();
            let height = usize::try_from(image.height).ok();
            let (Some(width), Some(height)) = (width, height) else {
                continue;
            };
            let Some(pixel_count) = width.checked_mul(height) else {
                continue;
            };
            if width == 0 || height == 0 || pixel_count > max_decompressed_size() / 3 {
                continue;
            }
            let filters = image.filters.as_deref().unwrap_or_default();
            let (name, data) = if filters.len() == 1 && filters[0] == "DCTDecode" {
                (
                    format!("page-{page_number}-image-{}.jpg", image_number + 1),
                    image.content.to_vec(),
                )
            } else if filters.is_empty() || filters.iter().any(|filter| filter == "FlateDecode") {
                let stream = document
                    .objects
                    .get(&image.id)
                    .and_then(|object| object.as_stream().ok());
                let Some(stream) = stream else {
                    continue;
                };
                let remaining = max_decompressed_size().saturating_sub(total_bytes);
                let mut decoded = match stream.get_plain_content_with_limit(remaining) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        web_sys::console::warn_1(&JsValue::from_str(&format!(
                            "[pdf] Skipping image on page {page_number}: {error}"
                        )));
                        continue;
                    }
                };
                let color_type = match image.color_space.as_deref() {
                    Some("DeviceGray") => 0,
                    Some("DeviceRGB") => 2,
                    _ => continue,
                };
                let bits = image.bits_per_component.unwrap_or(8);
                if bits != 8 {
                    continue;
                }
                if !pdf_apply_decode(&image, if color_type == 0 { 1 } else { 3 }, &mut decoded) {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "[pdf] Skipping image on page {page_number}: unsupported Decode array."
                    )));
                    continue;
                }
                let Some(png) = pdf_pixels_to_png(width, height, color_type, &decoded) else {
                    continue;
                };
                (
                    format!("page-{page_number}-image-{}.png", image_number + 1),
                    png,
                )
            } else {
                continue;
            };
            total_bytes = total_bytes.checked_add(data.len()).ok_or_else(|| {
                JsValue::from_str("PDF extracted contents exceed the 1 GiB browser limit.")
            })?;
            if total_bytes > max_decompressed_size() {
                return Err(JsValue::from_str(
                    "PDF extracted contents exceed the 1 GiB browser limit.",
                ));
            }
            files.push(&make_record(&name, &data)?);
        }
    }
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[pdf] Extraction complete: {} attachment/image file(s), {:.1} MiB in {:.1}s.",
        files.length(),
        total_bytes as f64 / 1_048_576.0,
        (js_sys::Date::now() - started) / 1000.0
    )));
    Ok(files)
}

/// Parse an NSIS installer and return its embedded files as JS objects.
/// Each object has `name`, `size`, and `data` (a Uint8Array) properties.
#[wasm_bindgen]
pub fn extract_nsis(input: &[u8]) -> Result<Array, JsValue> {
    let installer = nsis::NsisInstaller::builder(input)
        .max_decompressed_size(max_decompressed_size())
        .parse()
        .map_err(|error| JsValue::from_str(&format!("Could not read NSIS installer: {error}")))?;
    let files = Array::new();

    for (index, entry) in installer.files().enumerate() {
        let file = entry.map_err(|error| {
            JsValue::from_str(&format!(
                "Could not read embedded file {}: {error}",
                index + 1
            ))
        })?;
        let name = file
            .dest_path()
            .map(|path| path.to_install_path())
            .or_else(|_| file.name().map(|name| name.to_install_path()))
            .map_err(|error| JsValue::from_str(&format!("Could not read file name: {error}")))?;
        let data = file
            .decompress()
            .map_err(|error| JsValue::from_str(&format!("Could not decompress {name}: {error}")))?;

        files.push(&make_record(&name, &data)?);
    }

    Ok(files)
}

fn extract_inno_installer(
    installer: innospect::InnoInstaller<'_>,
    input_size: usize,
) -> Result<Array, JsValue> {
    let started = js_sys::Date::now();
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[inno] Parsing Inno Setup installer ({:.1} MiB).",
        input_size as f64 / 1_048_576.0
    )));
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[inno] Installer parsed: version {:?}, {} file record(s).",
        installer.version(),
        installer.files().len()
    )));
    if installer.is_encrypted() {
        return Err(JsValue::from_str(
            "This Inno Setup installer is password protected; password entry is not supported yet.",
        ));
    }

    let estimated_bytes = installer
        .files()
        .iter()
        .filter_map(|file| installer.file_location_for(file))
        .try_fold(0u64, |total, location| {
            total.checked_add(location.original_size)
        })
        .ok_or_else(|| JsValue::from_str("Inno Setup contents exceed the 1 GiB browser limit."))?;
    if estimated_bytes > max_decompressed_size() as u64 {
        return Err(JsValue::from_str(
            "Inno Setup contents exceed the 1 GiB browser limit.",
        ));
    }

    let files = Array::new();
    let mut total_bytes = 0usize;
    let mut count = 0usize;
    for extracted in installer.extract_files() {
        let (entry, data) = extracted.map_err(|error| {
            JsValue::from_str(&format!("Could not extract Inno Setup file: {error}"))
        })?;
        total_bytes = total_bytes.checked_add(data.len()).ok_or_else(|| {
            JsValue::from_str("Installer contents exceed the 1 GiB browser limit.")
        })?;
        if total_bytes > max_decompressed_size() {
            return Err(JsValue::from_str(
                "Installer contents exceed the 1 GiB browser limit.",
            ));
        }
        let source_name = entry
            .source
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(&entry.source);
        let name = if entry.destination.is_empty() {
            source_name
        } else {
            &entry.destination
        };
        files.push(&make_record(name, &data)?);
        count += 1;
        if count % 100 == 0 {
            web_sys::console::info_1(&JsValue::from_str(&format!(
                "[inno] Extracted {count} files ({:.1} MiB).",
                total_bytes as f64 / 1_048_576.0
            )));
        }
    }
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[inno] Extraction complete: {count} files, {:.1} MiB in {:.1}s.",
        total_bytes as f64 / 1_048_576.0,
        (js_sys::Date::now() - started) / 1000.0
    )));
    Ok(files)
}

fn extract_pyinstaller_cookie(input: &[u8], cookie_offset: usize) -> Result<Array, JsValue> {
    const COOKIE_MAGIC: &[u8; 8] = b"MEI\x0c\x0b\x0a\x0b\x0e";
    const COOKIE_SIZE: usize = 88;
    const TOC_ENTRY_HEADER_SIZE: usize = 18;

    let cookie_end = cookie_offset
        .checked_add(COOKIE_SIZE)
        .ok_or_else(|| JsValue::from_str("PyInstaller cookie offset overflow."))?;
    let cookie = input
        .get(cookie_offset..cookie_end)
        .ok_or_else(|| JsValue::from_str("PyInstaller archive cookie is truncated."))?;
    if cookie.get(..8) != Some(COOKIE_MAGIC.as_slice()) {
        return Err(JsValue::from_str("Invalid PyInstaller archive cookie."));
    }
    let read_be_u32 = |bytes: &[u8], offset: usize, what: &str| -> Result<u32, JsValue> {
        let field = bytes
            .get(offset..offset.saturating_add(4))
            .ok_or_else(|| JsValue::from_str(&format!("PyInstaller cookie is missing {what}.")))?;
        Ok(u32::from_be_bytes(field.try_into().unwrap()))
    };
    let package_len = read_be_u32(cookie, 8, "package length")? as usize;
    let toc_offset = read_be_u32(cookie, 12, "table offset")? as usize;
    let toc_len = read_be_u32(cookie, 16, "table length")? as usize;
    let archive_start = cookie_end
        .checked_sub(package_len)
        .ok_or_else(|| JsValue::from_str("Invalid PyInstaller package length."))?;
    let archive_end = archive_start
        .checked_add(package_len)
        .ok_or_else(|| JsValue::from_str("PyInstaller package range overflow."))?;
    if archive_end != cookie_end || archive_end > input.len() {
        return Err(JsValue::from_str(
            "PyInstaller package range is outside the executable.",
        ));
    }
    let toc_start = archive_start
        .checked_add(toc_offset)
        .ok_or_else(|| JsValue::from_str("PyInstaller table offset overflow."))?;
    let toc_end = toc_start
        .checked_add(toc_len)
        .ok_or_else(|| JsValue::from_str("PyInstaller table size overflow."))?;
    if toc_start < archive_start || toc_end > cookie_offset {
        return Err(JsValue::from_str(
            "PyInstaller table of contents is outside its package bounds.",
        ));
    }
    let toc = input
        .get(toc_start..toc_end)
        .ok_or_else(|| JsValue::from_str("PyInstaller table of contents is truncated."))?;

    let files = Array::new();
    let mut total_output = 0usize;
    let mut cursor = 0usize;
    let mut count = 0usize;
    while cursor < toc.len() {
        let header_end = cursor
            .checked_add(TOC_ENTRY_HEADER_SIZE)
            .ok_or_else(|| JsValue::from_str("PyInstaller table entry overflow."))?;
        let header = toc
            .get(cursor..header_end)
            .ok_or_else(|| JsValue::from_str("PyInstaller table entry header is truncated."))?;
        let entry_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let entry_end = cursor
            .checked_add(entry_len)
            .ok_or_else(|| JsValue::from_str("PyInstaller table entry size overflow."))?;
        if entry_len < TOC_ENTRY_HEADER_SIZE || entry_end > toc.len() {
            return Err(JsValue::from_str(
                "PyInstaller table entry has an invalid length.",
            ));
        }
        let data_offset = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
        let stored_len = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
        let uncompressed_len = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
        let compression = header[16];
        let typecode = header[17];
        if compression > 1 {
            return Err(JsValue::from_str(
                "PyInstaller archive entry uses an unknown compression flag.",
            ));
        }
        let raw_name = &toc[header_end..entry_end];
        let name_end = raw_name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(raw_name.len());
        let name = String::from_utf8_lossy(&raw_name[..name_end]).into_owned();
        cursor = entry_end;
        if typecode == b'o' || name.is_empty() {
            continue;
        }
        if uncompressed_len > max_decompressed_size() {
            return Err(JsValue::from_str(
                "PyInstaller contents exceed the 1 GiB browser limit.",
            ));
        }
        let data_start = archive_start
            .checked_add(data_offset)
            .ok_or_else(|| JsValue::from_str("PyInstaller file offset overflow."))?;
        let data_end = data_start
            .checked_add(stored_len)
            .ok_or_else(|| JsValue::from_str("PyInstaller file size overflow."))?;
        if data_start < archive_start || data_end > toc_start {
            return Err(JsValue::from_str(&format!(
                "PyInstaller entry {name} points outside the package data area."
            )));
        }
        let stored = input
            .get(data_start..data_end)
            .ok_or_else(|| JsValue::from_str("PyInstaller entry data is truncated."))?;
        let remaining = max_decompressed_size().saturating_sub(total_output);
        if uncompressed_len > remaining {
            return Err(JsValue::from_str(
                "PyInstaller contents exceed the 1 GiB browser limit.",
            ));
        }
        let data = if compression == 1 {
            let decoder = flate2::read::ZlibDecoder::new(stored);
            let mut data = Vec::with_capacity(uncompressed_len);
            decoder
                .take(uncompressed_len as u64 + 1)
                .read_to_end(&mut data)
                .map_err(|error| {
                    JsValue::from_str(&format!(
                        "Could not decompress PyInstaller entry {name}: {error}"
                    ))
                })?;
            if data.len() != uncompressed_len {
                return Err(JsValue::from_str(&format!(
                    "PyInstaller entry {name} decompressed to an unexpected size."
                )));
            }
            data
        } else {
            if stored_len != uncompressed_len {
                return Err(JsValue::from_str(&format!(
                    "Uncompressed PyInstaller entry {name} has inconsistent size fields."
                )));
            }
            stored.to_vec()
        };
        total_output = total_output
            .checked_add(data.len())
            .ok_or_else(|| JsValue::from_str("PyInstaller contents size overflow."))?;
        files.push(&make_record(&name, &data)?);
        count += 1;
        if count % 100 == 0 {
            web_sys::console::info_1(&JsValue::from_str(&format!(
                "[pyinstaller] Extracted {count} entries ({:.1} MiB).",
                total_output as f64 / 1_048_576.0
            )));
        }
    }
    Ok(files)
}

/// Extract the top-level entries from a PyInstaller CArchive.
/// Embedded PYZ archives are returned as files and are not traversed.
#[wasm_bindgen]
pub fn extract_pyinstaller(input: &[u8]) -> Result<Array, JsValue> {
    const COOKIE_MAGIC: &[u8; 8] = b"MEI\x0c\x0b\x0a\x0b\x0e";
    let started = js_sys::Date::now();
    let mut last_error = None;
    let mut candidates = 0usize;
    for (offset, marker) in input.windows(COOKIE_MAGIC.len()).enumerate().rev() {
        if marker != COOKIE_MAGIC {
            continue;
        }
        candidates += 1;
        match extract_pyinstaller_cookie(input, offset) {
            Ok(files) if files.length() > 0 => {
                web_sys::console::info_1(&JsValue::from_str(&format!(
                    "[pyinstaller] Extraction complete: {} top-level entries in {:.1}s; embedded PYZ archives remain opaque.",
                    files.length(),
                    (js_sys::Date::now() - started) / 1000.0
                )));
                return Ok(files);
            }
            Ok(_) => {
                last_error = Some("PyInstaller archive contains no extractable entries.".to_owned())
            }
            Err(error) => {
                last_error = error.as_string();
            }
        }
        if candidates >= 32 {
            break;
        }
    }
    Err(JsValue::from_str(last_error.as_deref().unwrap_or(
        "Could not find a supported PyInstaller archive cookie.",
    )))
}

/// Extract a format provided by exav-unpack that is not handled by a dedicated reader.
/// The no-emulator feature preset leaves static unpacking enabled without executing PE stubs.
#[wasm_bindgen]
pub fn extract_exav(input: &[u8]) -> Result<Array, JsValue> {
    use exav_unpack::{Budget, Format, Limits, detect, extract};

    let format = if exav_unpack::is_upx(input) {
        Some(Format::Upx)
    } else if exav_unpack::is_pepack(input) {
        Some(Format::PePacked)
    } else {
        detect(&input)
    }
    .ok_or_else(|| JsValue::from_str("exav-unpack did not recognize a supported format."))?;

    let mut limits = Limits::default();
    limits.max_extracted_bytes = max_decompressed_size() as _;
    limits.max_buffer_bytes = max_decompressed_size() as _;
    let mut budget = Budget::new(limits);
    let entries = extract(format, &input, &mut budget).map_err(|error| {
        JsValue::from_str(&format!("Could not extract with exav-unpack: {error}"))
    })?;
    let files = Array::new();
    let mut total_bytes = 0usize;
    for entry in entries {
        total_bytes = total_bytes.checked_add(entry.data.len()).ok_or_else(|| {
            JsValue::from_str("Extracted contents exceed the 1 GiB browser limit.")
        })?;
        if total_bytes > max_decompressed_size() {
            return Err(JsValue::from_str(
                "Extracted contents exceed the 1 GiB browser limit.",
            ));
        }
        files.push(&make_record(&entry.name, &entry.data)?);
    }
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[exav] Extracted {} files ({:.1} MiB) using {format:?}.",
        files.length(),
        total_bytes as f64 / 1_048_576.0
    )));
    Ok(files)
}

/// Extract regular files from a Type 2 AppImage's embedded SquashFS filesystem.
#[wasm_bindgen]
pub fn extract_appimage(input: &[u8]) -> Result<Array, JsValue> {
    use squashfs_reader::{FileSystem, FileType};

    const MAX_ENTRIES: usize = 100_000;
    if !input.starts_with(b"\x7fELF") || input.get(8..11) != Some(b"AI\x02") {
        return Err(JsValue::from_str(
            "This is not a supported Type 2 AppImage.",
        ));
    }

    let elf = goblin::elf::Elf::parse(input).map_err(|error| {
        JsValue::from_str(&format!("Could not parse AppImage ELF runtime: {error}"))
    })?;
    let section_table_end = elf
        .header
        .e_shoff
        .checked_add(u64::from(elf.header.e_shentsize) * u64::from(elf.header.e_shnum))
        .ok_or_else(|| JsValue::from_str("AppImage ELF section table overflows."))?;
    let last_section_end = elf
        .section_headers
        .iter()
        .filter(|section| section.sh_type != goblin::elf::section_header::SHT_NOBITS)
        .try_fold(0u64, |end, section| {
            section
                .sh_offset
                .checked_add(section.sh_size)
                .map(|section_end| end.max(section_end))
        })
        .ok_or_else(|| JsValue::from_str("AppImage ELF section range overflows."))?;
    let squashfs_offset = usize::try_from(section_table_end.max(last_section_end))
        .map_err(|_| JsValue::from_str("AppImage payload offset is too large."))?;
    let Some(squashfs) = input.get(squashfs_offset..) else {
        return Err(JsValue::from_str(
            "AppImage has no payload after its ELF runtime.",
        ));
    };
    if !squashfs.starts_with(b"hsqs") {
        return Err(JsValue::from_str(
            "AppImage runtime was found, but its payload is not a supported SquashFS image.",
        ));
    }

    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[appimage] Opening embedded SquashFS payload at byte {squashfs_offset}."
    )));
    let filesystem = FileSystem::from_read(Cursor::new(squashfs)).map_err(|error| {
        JsValue::from_str(&format!(
            "Could not open AppImage SquashFS payload: {error}"
        ))
    })?;
    let files = Array::new();
    let mut pending_directories = vec![String::new()];
    let mut total_bytes = 0usize;
    let mut entry_count = 0usize;
    let mut symlink_count = 0usize;

    while let Some(parent) = pending_directories.pop() {
        let directory_path = if parent.is_empty() {
            "/".to_owned()
        } else {
            format!("/{parent}")
        };
        let entries = filesystem.read_dir(&directory_path).map_err(|error| {
            JsValue::from_str(&format!(
                "Could not read AppImage directory {directory_path}: {error}"
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                JsValue::from_str(&format!("Could not read AppImage directory entry: {error}"))
            })?;
            entry_count += 1;
            if entry_count > MAX_ENTRIES {
                return Err(JsValue::from_str(
                    "AppImage contains more than 100,000 filesystem entries.",
                ));
            }
            let name = entry.name();
            if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                continue;
            }
            // Backslashes are treated as path separators by the download UI, so
            // keep a Linux filename from becoming an unexpected path component.
            let name = name.replace('\\', "_");
            let relative_path = if parent.is_empty() {
                name
            } else {
                format!("{parent}/{name}")
            };
            match entry.ty() {
                FileType::Symlink => {
                    symlink_count += 1;
                    continue;
                }
                FileType::Other => continue,
                FileType::Directory | FileType::File => {}
            }
            let metadata = entry.metadata(&filesystem).map_err(|error| {
                JsValue::from_str(&format!(
                    "Could not read AppImage metadata for {relative_path}: {error}"
                ))
            })?;
            if metadata.is_dir() {
                pending_directories.push(relative_path);
                continue;
            }
            if metadata.is_symlink() {
                symlink_count += 1;
                continue;
            }
            if !metadata.is_file() {
                continue;
            }

            let declared_size = usize::try_from(metadata.len()).map_err(|_| {
                JsValue::from_str(&format!(
                    "AppImage file {relative_path} is too large to extract."
                ))
            })?;
            let remaining = max_decompressed_size().saturating_sub(total_bytes);
            if declared_size > remaining {
                return Err(JsValue::from_str(&format!(
                    "AppImage contents exceed the configured {} MiB extraction limit.",
                    max_decompressed_size() / (1024 * 1024)
                )));
            }
            let mut reader = metadata.read_file(&filesystem).map_err(|error| {
                JsValue::from_str(&format!(
                    "Could not open AppImage file {relative_path}: {error}"
                ))
            })?;
            let mut data = Vec::new();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let read = reader.read(&mut chunk).map_err(|error| {
                    JsValue::from_str(&format!(
                        "Could not extract AppImage file {relative_path}: {error}"
                    ))
                })?;
                if read == 0 {
                    break;
                }
                if data.len().saturating_add(read) > declared_size {
                    return Err(JsValue::from_str(&format!(
                        "AppImage file {relative_path} exceeded its declared size."
                    )));
                }
                data.extend_from_slice(&chunk[..read]);
            }
            if data.len() != declared_size {
                return Err(JsValue::from_str(&format!(
                    "AppImage file {relative_path} did not match its declared size."
                )));
            }
            total_bytes += data.len();
            files.push(&make_record(&relative_path, &data)?);
        }
    }

    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[appimage] Extracted {} regular files ({:.1} MiB); skipped {symlink_count} symlink(s).",
        files.length(),
        total_bytes as f64 / 1_048_576.0
    )));
    Ok(files)
}

/// Extract the application payload from a Debian package.
/// Debian packages are ar files; their `data.tar.*` member contains installed files.
#[wasm_bindgen]
pub fn extract_deb(input: &[u8]) -> Result<Array, JsValue> {
    use exav_unpack::{Budget, Format, Limits, extract};

    if !input.starts_with(b"!<arch>\n") {
        return Err(JsValue::from_str("This is not a Debian ar package."));
    }
    let started = js_sys::Date::now();
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[deb] Reading package container ({:.1} MiB).",
        input.len() as f64 / 1_048_576.0
    )));

    let mut limits = Limits::default();
    limits.max_extracted_bytes = max_decompressed_size() as _;
    limits.max_buffer_bytes = max_decompressed_size() as _;
    let mut budget = Budget::new(limits);
    let members = extract(Format::Ar, &input, &mut budget).map_err(|error| {
        JsValue::from_str(&format!("Could not read Debian package members: {error}"))
    })?;
    let mut data_member = None;
    for member in members {
        if member.name == "data.tar" || member.name.starts_with("data.tar.") {
            if data_member.is_some() {
                return Err(JsValue::from_str(
                    "Debian package contains more than one data.tar payload.",
                ));
            }
            data_member = Some((member.name, member.data));
        }
    }
    let Some((name, data)) = data_member else {
        return Err(JsValue::from_str(
            "Debian package has no supported data.tar payload.",
        ));
    };

    let files = extract_archive(&data, &name)?;
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[deb] Extracted {} application file(s) from {name} in {:.1}s.",
        files.length(),
        (js_sys::Date::now() - started) / 1000.0
    )));
    Ok(files)
}

#[derive(Debug)]
struct BurnPayload {
    source_path: String,
    file_path: String,
    container: String,
}

fn burn_xml_attributes(
    event: &quick_xml::events::BytesStart<'_>,
    reader: &quick_xml::Reader<&[u8]>,
) -> Result<HashMap<String, String>, JsValue> {
    let mut attributes = HashMap::new();
    for attribute in event.attributes() {
        let attribute = attribute.map_err(|error| {
            JsValue::from_str(&format!("Invalid Burn manifest attribute: {error}"))
        })?;
        let key = std::str::from_utf8(attribute.key.as_ref())
            .map_err(|error| {
                JsValue::from_str(&format!("Invalid Burn manifest attribute name: {error}"))
            })?
            .to_owned();
        let value = attribute
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|error| {
                JsValue::from_str(&format!("Invalid Burn manifest attribute value: {error}"))
            })?
            .into_owned();
        attributes.insert(key, value);
    }
    Ok(attributes)
}

fn parse_burn_manifest(
    manifest: &[u8],
) -> Result<(HashMap<String, usize>, Vec<BurnPayload>), JsValue> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_reader(manifest);
    let mut buffer = Vec::new();
    let mut containers = HashMap::new();
    let mut payloads = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                let tag = event.local_name();
                let tag = tag.as_ref();
                if tag == b"Container" {
                    let attributes = burn_xml_attributes(&event, &reader)?;
                    if attributes
                        .get("Attached")
                        .is_some_and(|value| value == "yes")
                    {
                        if let (Some(id), Some(index)) = (
                            attributes.get("Id"),
                            attributes
                                .get("AttachedIndex")
                                .and_then(|value| value.parse().ok()),
                        ) {
                            containers.insert(id.clone(), index);
                        }
                    }
                } else if tag == b"Payload" {
                    let attributes = burn_xml_attributes(&event, &reader)?;
                    if attributes
                        .get("Packaging")
                        .is_some_and(|value| value == "embedded")
                    {
                        if let (Some(source_path), Some(file_path), Some(container)) = (
                            attributes.get("SourcePath"),
                            attributes.get("FilePath"),
                            attributes.get("Container"),
                        ) {
                            payloads.push(BurnPayload {
                                source_path: source_path.clone(),
                                file_path: file_path.clone(),
                                container: container.clone(),
                            });
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(JsValue::from_str(&format!(
                    "Could not parse WiX Burn manifest: {error}"
                )));
            }
        }
        buffer.clear();
    }
    if containers.is_empty() {
        return Err(JsValue::from_str(
            "WiX Burn manifest has no attached payload containers.",
        ));
    }
    Ok((containers, payloads))
}

fn burn_output_path(path: &str) -> Result<String, JsValue> {
    let path = path.replace('\\', "/");
    if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') {
        return Err(JsValue::from_str(
            "WiX Burn manifest contains an absolute payload path.",
        ));
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(JsValue::from_str(
                    "WiX Burn manifest contains a parent-directory payload path.",
                ));
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(JsValue::from_str(
            "WiX Burn manifest contains an empty payload path.",
        ));
    }
    Ok(parts.join("/"))
}

fn extract_burn_cabinet(
    cabinet_bytes: &[u8],
    payloads: &[BurnPayload],
    total_bytes: &mut usize,
    output: &Array,
) -> Result<usize, JsValue> {
    let mut cabinet = cab::Cabinet::new(Cursor::new(cabinet_bytes)).map_err(|error| {
        JsValue::from_str(&format!("Could not read WiX Burn CAB container: {error}"))
    })?;
    let mut extracted = 0;
    for payload in payloads {
        let output_name = burn_output_path(&payload.file_path)?;
        let Some(entry_name) = cabinet
            .folder_entries()
            .flat_map(|folder| folder.file_entries())
            .find(|entry| entry.name().eq_ignore_ascii_case(&payload.source_path))
            .map(|entry| entry.name().to_owned())
        else {
            continue;
        };
        let size = cabinet
            .get_file_entry(&entry_name)
            .expect("entry name came from the cabinet")
            .uncompressed_size() as usize;
        let remaining = max_decompressed_size().saturating_sub(*total_bytes);
        if size > remaining {
            return Err(JsValue::from_str(
                "WiX Burn payloads exceed the 1 GiB browser extraction limit.",
            ));
        }
        let mut data = Vec::with_capacity(size);
        cabinet
            .read_file(&entry_name)
            .and_then(|reader| reader.take(remaining as u64 + 1).read_to_end(&mut data))
            .map_err(|error| {
                JsValue::from_str(&format!(
                    "Could not extract WiX Burn payload {output_name}: {error}"
                ))
            })?;
        if data.len() != size {
            return Err(JsValue::from_str(&format!(
                "WiX Burn payload {output_name} has an unexpected decompressed size."
            )));
        }
        *total_bytes += data.len();
        output.push(&make_record(&output_name, &data)?);
        extracted += 1;
    }
    Ok(extracted)
}

/// Extract embedded payloads from a WiX Burn executable bundle.
/// Returns None only when the input is not a Burn bundle.
fn try_extract_wix_burn(input: &[u8]) -> Option<Result<Array, JsValue>> {
    const BURN_SECTION_MAGIC: u32 = 0x00F1_4300;
    const BURN_SECTION_VERSION: u32 = 2;
    const BURN_CONTAINER_CABINET: u32 = 1;
    const BURN_CONTAINER_SEVENZIP: u32 = 2;
    const MAX_BURN_MANIFEST_SIZE: usize = 16 * 1024 * 1024;

    let pe = match goblin::pe::PE::parse(input) {
        Ok(pe) => pe,
        Err(_) => return None,
    };
    let section = pe
        .sections
        .iter()
        .find(|section| &section.name == b".wixburn")?;
    let section_data = match section.data(input) {
        Ok(Some(data)) => data,
        Ok(None) => return Some(Err(JsValue::from_str("WiX Burn section has no file data."))),
        Err(error) => {
            return Some(Err(JsValue::from_str(&format!(
                "Could not read WiX Burn section: {error}"
            ))));
        }
    };
    let data = section_data.as_ref();
    let read_u32 = |offset: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            data.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    if read_u32(0) != Some(BURN_SECTION_MAGIC) || read_u32(4) != Some(BURN_SECTION_VERSION) {
        return Some(Err(JsValue::from_str(
            "Unsupported or malformed WiX Burn section.",
        )));
    }
    let Some(stub_size) = read_u32(24).map(|value| value as usize) else {
        return Some(Err(JsValue::from_str("Truncated WiX Burn section.")));
    };
    let original_signature_offset = read_u32(32).unwrap_or(0) as usize;
    let original_signature_size = read_u32(36).unwrap_or(0) as usize;
    let Some(format) = read_u32(40) else {
        return Some(Err(JsValue::from_str("Truncated WiX Burn section.")));
    };
    let Some(container_count) = read_u32(44).map(|value| value as usize) else {
        return Some(Err(JsValue::from_str("Truncated WiX Burn section.")));
    };
    let Some(container_table_size) = container_count.checked_mul(4) else {
        return Some(Err(JsValue::from_str("Invalid WiX Burn container table.")));
    };
    let Some(sizes_end) = 48usize.checked_add(container_table_size) else {
        return Some(Err(JsValue::from_str("Invalid WiX Burn container table.")));
    };
    if container_count == 0 || sizes_end > data.len() {
        return Some(Err(JsValue::from_str(
            "Invalid WiX Burn container count or table bounds.",
        )));
    }
    let sizes: Vec<usize> = (0..container_count)
        .map(|index| read_u32(48 + index * 4).unwrap_or(0) as usize)
        .collect();
    if sizes.iter().any(|size| *size == 0) {
        return Some(Err(JsValue::from_str(
            "WiX Burn bundle has an empty container entry.",
        )));
    }
    if format != BURN_CONTAINER_CABINET && format != BURN_CONTAINER_SEVENZIP {
        return Some(Err(JsValue::from_str(
            "This WiX Burn bundle uses an unsupported container format.",
        )));
    }

    let engine_end = if original_signature_offset != 0 {
        original_signature_offset.checked_add(original_signature_size)
    } else if let Some(certificate) = pe
        .header
        .optional_header
        .as_ref()
        .and_then(|header| header.data_directories.get_certificate_table())
    {
        (certificate.virtual_address as usize).checked_add(certificate.size as usize)
    } else {
        stub_size.checked_add(sizes[0])
    };
    let Some(engine_end) = engine_end else {
        return Some(Err(JsValue::from_str("Invalid WiX Burn engine boundary.")));
    };
    let Some(ux_end) = stub_size.checked_add(sizes[0]) else {
        return Some(Err(JsValue::from_str(
            "Invalid WiX Burn UX container boundary.",
        )));
    };
    if ux_end > input.len() {
        return Some(Err(JsValue::from_str(
            "WiX Burn UX container extends beyond the executable.",
        )));
    }
    let ux_bytes = &input[stub_size..ux_end];
    let mut ux_cabinet = match cab::Cabinet::new(Cursor::new(ux_bytes)) {
        Ok(cabinet) => cabinet,
        Err(error) => {
            return Some(Err(JsValue::from_str(&format!(
                "Could not read WiX Burn bootstrapper container: {error}"
            ))));
        }
    };
    let manifest_names: Vec<String> = ux_cabinet
        .folder_entries()
        .flat_map(|folder| folder.file_entries().map(|entry| entry.name().to_owned()))
        .collect();
    let mut manifest = None;
    for name in manifest_names {
        let Some(size) = ux_cabinet
            .get_file_entry(&name)
            .map(|entry| entry.uncompressed_size() as usize)
        else {
            continue;
        };
        if size == 0 || size > MAX_BURN_MANIFEST_SIZE {
            continue;
        }
        let mut bytes = Vec::with_capacity(size);
        if ux_cabinet
            .read_file(&name)
            .and_then(|reader| {
                reader
                    .take(MAX_BURN_MANIFEST_SIZE as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .is_ok()
            && bytes.len() <= MAX_BURN_MANIFEST_SIZE
            && bytes
                .windows(b"BurnManifest".len())
                .any(|window| window == b"BurnManifest")
        {
            manifest = Some(bytes);
            break;
        }
    }
    drop(ux_cabinet);
    let Some(manifest) = manifest else {
        return Some(Err(JsValue::from_str(
            "Could not find the WiX Burn manifest in its bootstrapper container.",
        )));
    };
    let (container_indexes, payloads) = match parse_burn_manifest(&manifest) {
        Ok(parsed) => parsed,
        Err(error) => return Some(Err(error)),
    };

    let output = Array::new();
    let mut total_bytes = 0usize;
    let mut extracted_count = 0usize;
    let mut container_offset = engine_end;
    for (index, container_size) in sizes.iter().copied().enumerate().skip(1) {
        let Some(container_end) = container_offset.checked_add(container_size) else {
            return Some(Err(JsValue::from_str(
                "WiX Burn container boundary overflow.",
            )));
        };
        let Some(container_bytes) = input.get(container_offset..container_end) else {
            // Some bundles omit attached containers whose payloads are downloaded on demand.
            container_offset = container_end;
            continue;
        };
        let relevant: Vec<&BurnPayload> = payloads
            .iter()
            .filter(|payload| container_indexes.get(&payload.container) == Some(&index))
            .collect();
        if !relevant.is_empty() {
            let cabinet_payloads: Vec<BurnPayload> = relevant
                .into_iter()
                .map(|payload| BurnPayload {
                    source_path: payload.source_path.clone(),
                    file_path: payload.file_path.clone(),
                    container: payload.container.clone(),
                })
                .collect();
            let count = if format == BURN_CONTAINER_CABINET {
                match extract_burn_cabinet(
                    container_bytes,
                    &cabinet_payloads,
                    &mut total_bytes,
                    &output,
                ) {
                    Ok(count) => count,
                    Err(error) => return Some(Err(error)),
                }
            } else {
                let entries = match extract_archive(container_bytes, "burn-container.7z") {
                    Ok(entries) => entries,
                    Err(error) => return Some(Err(error)),
                };
                let mut count = 0;
                for payload in &cabinet_payloads {
                    let name = match burn_output_path(&payload.file_path) {
                        Ok(name) => name,
                        Err(error) => return Some(Err(error)),
                    };
                    for entry_index in 0..entries.length() {
                        let entry = entries.get(entry_index);
                        let entry_name = match Reflect::get(&entry, &"name".into())
                            .ok()
                            .and_then(|value| value.as_string())
                        {
                            Some(name) => name,
                            None => {
                                return Some(Err(JsValue::from_str(
                                    "Invalid entry returned from WiX Burn 7-Zip container.",
                                )));
                            }
                        };
                        if !entry_name.eq_ignore_ascii_case(&payload.source_path) {
                            continue;
                        }
                        let data_value = match Reflect::get(&entry, &"data".into()) {
                            Ok(value) => value,
                            Err(error) => return Some(Err(error)),
                        };
                        let data = Uint8Array::new(&data_value).to_vec();
                        let Some(new_total) = total_bytes.checked_add(data.len()) else {
                            return Some(Err(JsValue::from_str("WiX Burn payload size overflow.")));
                        };
                        total_bytes = new_total;
                        if total_bytes > max_decompressed_size() {
                            return Some(Err(JsValue::from_str(
                                "WiX Burn payloads exceed the 1 GiB browser extraction limit.",
                            )));
                        }
                        let record = match make_record(&name, &data) {
                            Ok(record) => record,
                            Err(error) => return Some(Err(error)),
                        };
                        output.push(&record);
                        count += 1;
                    }
                }
                count
            };
            extracted_count += count;
        }
        container_offset = container_end;
    }
    if extracted_count == 0 {
        return Some(Err(JsValue::from_str(
            "No embedded WiX Burn payloads could be extracted. Detached or download-only payloads are not fetched.",
        )));
    }
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[wix-burn] Extracted {extracted_count} embedded payload(s) ({:.1} MiB).",
        total_bytes as f64 / 1_048_576.0
    )));
    Some(Ok(output))
}

/// Try the supported non-MSI Windows installer formats in a safe order.
/// Inno and SFX archive signatures are checked before the existing NSIS parser.
#[wasm_bindgen]
pub fn extract_exe_installer(input: &[u8]) -> Result<Array, JsValue> {
    if let Some(result) = try_extract_wix_burn(input) {
        return result;
    }
    if let Ok(installer) = innospect::InnoInstaller::from_bytes(input) {
        return extract_inno_installer(installer, input.len());
    }
    if input
        .windows(8)
        .any(|window| window == b"MEI\x0c\x0b\x0a\x0b\x0e")
    {
        return extract_pyinstaller(input);
    }

    let mut archive_error = None;
    if unarc_rs::unified::ArchiveFormat::detect_from_bytes(input).is_some() {
        match extract_archive(input, "setup.exe") {
            Ok(files) if files.length() > 0 => return Ok(files),
            Ok(_) => {}
            Err(error) => archive_error = Some(error),
        }
    }

    // SFX archives place their archive signature after a PE stub, so scan for
    // the common ZIP, 7z, CAB, and ARJ signatures and try each candidate offset.
    let mut candidates = 0;
    for (offset, marker) in input.windows(2).enumerate() {
        let is_arj = marker == b"\x60\xea";
        if is_arj {
            match extract_archive(&input[offset..], "setup.arj") {
                Ok(files) if files.length() > 0 => return Ok(files),
                Ok(_) => {}
                Err(error) => archive_error = Some(error),
            }
            candidates += 1;
            if candidates >= 32 {
                break;
            }
            continue;
        }
        let Some(marker) = input.get(offset..offset.saturating_add(4)) else {
            continue;
        };
        let is_zip = marker == b"PK\x03\x04";
        let is_cab = marker == b"MSCF";
        let is_7z = marker == b"7z\xBC\xAF"
            && input.get(offset + 4) == Some(&0x27)
            && input.get(offset + 5) == Some(&0x1c);
        if !is_zip && !is_cab && !is_7z {
            continue;
        }
        match extract_archive(&input[offset..], "setup.exe") {
            Ok(files) if files.length() > 0 => return Ok(files),
            Ok(_) => {}
            Err(error) => archive_error = Some(error),
        }
        candidates += 1;
        if candidates >= 32 {
            break;
        }
    }
    if let Ok(files) = extract_exav(input) {
        if files.length() > 0 {
            return Ok(files);
        }
    }
    match extract_nsis(input) {
        Ok(files) => Ok(files),
        Err(error) => Err(archive_error.unwrap_or(error)),
    }
}

/// Extract a ZIP, TAR (optionally compressed), or 7z archive in memory.
/// The filename is used for archive types such as compressed TAR streams.
#[wasm_bindgen]
pub fn extract_archive(input: &[u8], filename: &str) -> Result<Array, JsValue> {
    use std::path::Path;
    use unarc_rs::unified::{ArchiveFormat, ArchiveOptions};

    let started = js_sys::Date::now();
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[archive] Opening {filename} ({:.1} MiB).",
        input.len() as f64 / 1_048_576.0
    )));
    let format = ArchiveFormat::from_path(Path::new(filename))
        .or_else(|| ArchiveFormat::detect_from_bytes(input))
        .ok_or_else(|| JsValue::from_str("Could not identify a supported archive format."))?;
    let options = ArchiveOptions::new()
        .with_max_entry_size(Some(max_decompressed_size() as u64))
        .with_max_total_size(Some(max_decompressed_size() as u64));
    let mut archive = format
        .open_with_options(Cursor::new(input), options)
        .map_err(|error| JsValue::from_str(&format!("Could not open archive: {error}")))?;

    let files = Array::new();
    let mut total_bytes = 0usize;
    let mut count = 0usize;
    while let Some(entry) = archive
        .next_entry()
        .map_err(|error| JsValue::from_str(&format!("Could not read archive entry: {error}")))?
    {
        let name = entry.name().replace('\\', "/");
        if name.is_empty() || name.ends_with('/') {
            continue;
        }
        let data = archive
            .read(&entry)
            .map_err(|error| JsValue::from_str(&format!("Could not extract {name}: {error}")))?;
        total_bytes = total_bytes
            .checked_add(data.len())
            .ok_or_else(|| JsValue::from_str("Archive contents exceed the 1 GiB browser limit."))?;
        if total_bytes > max_decompressed_size() {
            return Err(JsValue::from_str(
                "Archive contents exceed the 1 GiB browser limit.",
            ));
        }
        files.push(&make_record(&name, &data)?);
        count += 1;
        if count % 100 == 0 {
            web_sys::console::info_1(&JsValue::from_str(&format!(
                "[archive] Extracted {count} files ({:.1} MiB).",
                total_bytes as f64 / 1_048_576.0
            )));
        }
    }
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[archive] Finished {filename}: {count} files, {:.1} MiB in {:.1}s.",
        total_bytes as f64 / 1_048_576.0,
        (js_sys::Date::now() - started) / 1000.0
    )));
    Ok(files)
}

/// Extract all regular files from an ISO9660 image.
#[wasm_bindgen]
pub fn extract_iso(input: &[u8]) -> Result<Array, JsValue> {
    use gpt_disk_io::{BlockIoAdapter, gpt_disk_types::BlockSize};
    use iso9660::DirectoryIterator;
    use std::collections::HashSet;

    const ISO_BLOCK_SIZE: usize = 2048;
    let started = js_sys::Date::now();
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[iso] Opening ISO image ({:.1} MiB).",
        input.len() as f64 / 1_048_576.0
    )));
    let block_size = BlockSize::new(ISO_BLOCK_SIZE as u32)
        .ok_or_else(|| JsValue::from_str("Could not initialize ISO sector size."))?;
    let mut block_io = BlockIoAdapter::new(input, block_size);
    let volume = iso9660::mount(&mut block_io, 0)
        .map_err(|error| JsValue::from_str(&format!("Could not read ISO9660 image: {error}")))?;

    let files = Array::new();
    let mut dirs = vec![(
        String::new(),
        volume.root_extent_lba,
        volume.root_extent_len,
    )];
    let mut visited = HashSet::new();
    let mut total_bytes = 0usize;
    let mut count = 0usize;
    while let Some((parent, lba, len)) = dirs.pop() {
        if !visited.insert((lba, len)) {
            continue;
        }
        let directory_end = (lba as u64)
            .checked_mul(ISO_BLOCK_SIZE as u64)
            .and_then(|start| start.checked_add(len as u64))
            .ok_or_else(|| JsValue::from_str("Invalid ISO directory extent."))?;
        if len as usize > 64 * 1024 * 1024 || directory_end > input.len() as u64 {
            return Err(JsValue::from_str(
                "ISO directory extent is outside the supported image bounds.",
            ));
        }
        let mut directory_entries = Vec::new();
        for item in DirectoryIterator::new(&mut block_io, lba, len) {
            directory_entries.push(item.map_err(|error| {
                JsValue::from_str(&format!("Could not read ISO directory: {error}"))
            })?);
        }
        for entry in directory_entries {
            let component = entry.name.trim_end_matches('.');
            if component.is_empty() || component == "." || component == ".." {
                continue;
            }
            let path = if parent.is_empty() {
                component.to_owned()
            } else {
                format!("{parent}/{component}")
            };
            if entry.flags.directory {
                dirs.push((path, entry.extent_lba, entry.data_length));
                continue;
            }
            let size = usize::try_from(entry.size)
                .map_err(|_| JsValue::from_str("ISO file is too large for this browser."))?;
            total_bytes = total_bytes
                .checked_add(size)
                .ok_or_else(|| JsValue::from_str("ISO contents exceed the 1 GiB browser limit."))?;
            if total_bytes > max_decompressed_size() {
                return Err(JsValue::from_str(
                    "ISO contents exceed the 1 GiB browser limit.",
                ));
            }
            let data = iso9660::read_file_vec(&mut block_io, &entry).map_err(|error| {
                JsValue::from_str(&format!("Could not extract ISO file {path}: {error}"))
            })?;
            files.push(&make_record(&path, &data)?);
            count += 1;
        }
    }
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[iso] Extraction complete: {count} files, {:.1} MiB in {:.1}s.",
        total_bytes as f64 / 1_048_576.0,
        (js_sys::Date::now() - started) / 1000.0
    )));
    Ok(files)
}

fn read_boot_u32(input: &[u8], offset: usize, label: &str) -> Result<u32, JsValue> {
    let bytes = input
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| JsValue::from_str(&format!("Android boot image is missing {label}.")))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn align_boot_offset(offset: usize, page_size: usize) -> Result<usize, JsValue> {
    offset
        .checked_add(page_size - 1)
        .map(|value| value / page_size * page_size)
        .ok_or_else(|| JsValue::from_str("Android boot image offset overflow."))
}

fn append_boot_component(
    files: &Array,
    input: &[u8],
    name: &str,
    offset: usize,
    size: usize,
    total_size: &mut usize,
) -> Result<(), JsValue> {
    if size == 0 {
        return Ok(());
    }
    let end = offset
        .checked_add(size)
        .ok_or_else(|| JsValue::from_str("Android boot image component size overflow."))?;
    let data = input.get(offset..end).ok_or_else(|| {
        JsValue::from_str(&format!(
            "Android boot image component {name} is outside the image."
        ))
    })?;
    *total_size = total_size
        .checked_add(data.len())
        .ok_or_else(|| JsValue::from_str("Android boot image exceeds the 1 GiB browser limit."))?;
    if *total_size > max_decompressed_size() {
        return Err(JsValue::from_str(
            "Android boot image contents exceed the 1 GiB browser limit.",
        ));
    }
    files.push(&make_record(name, data)?);
    Ok(())
}

/// Extract the payload components from an Android boot image (header v0–v4).
#[wasm_bindgen]
pub fn extract_android_boot(input: &[u8]) -> Result<Array, JsValue> {
    const MAGIC: &[u8; 8] = b"ANDROID!";
    const MODERN_PAGE_SIZE: usize = 4096;
    if !input.starts_with(MAGIC) {
        return Err(JsValue::from_str("This is not an Android boot image."));
    }
    if input.len() < 44 {
        return Err(JsValue::from_str("Android boot image header is truncated."));
    }

    let version = read_boot_u32(input, 40, "header version")?;
    if version > 4 {
        return Err(JsValue::from_str(&format!(
            "Android boot image header version {version} is not supported yet."
        )));
    }
    let (
        kernel_size,
        ramdisk_size,
        second_size,
        page_size,
        recovery_size,
        recovery_offset,
        dtb_size,
        signature_size,
    ) = if version < 3 {
        let page_size = read_boot_u32(input, 36, "page size")? as usize;
        if !(2048..=65536).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(JsValue::from_str(
                "Android boot image has an invalid page size.",
            ));
        }
        let recovery_size = if version >= 1 {
            read_boot_u32(input, 1632, "recovery DTBO size")? as usize
        } else {
            0
        };
        let recovery_offset = if version >= 1 {
            usize::try_from(u64::from_le_bytes(
                input
                    .get(1636..1644)
                    .ok_or_else(|| JsValue::from_str("Android boot image header is truncated."))?
                    .try_into()
                    .unwrap(),
            ))
            .map_err(|_| JsValue::from_str("Android recovery DTBO offset is too large."))?
        } else {
            0
        };
        let dtb_size = if version >= 2 {
            read_boot_u32(input, 1648, "DTB size")? as usize
        } else {
            0
        };
        (
            read_boot_u32(input, 8, "kernel size")? as usize,
            read_boot_u32(input, 16, "ramdisk size")? as usize,
            read_boot_u32(input, 24, "second-stage size")? as usize,
            page_size,
            recovery_size,
            recovery_offset,
            dtb_size,
            0,
        )
    } else {
        let header_size = read_boot_u32(input, 20, "header size")? as usize;
        let required_header_size = if version == 4 { 1584 } else { 1580 };
        if header_size < required_header_size || header_size > MODERN_PAGE_SIZE {
            return Err(JsValue::from_str(
                "Android boot image has an invalid header size.",
            ));
        }
        (
            read_boot_u32(input, 8, "kernel size")? as usize,
            read_boot_u32(input, 12, "ramdisk size")? as usize,
            0,
            MODERN_PAGE_SIZE,
            0,
            0,
            0,
            if version == 4 {
                read_boot_u32(input, 1580, "boot signature size")? as usize
            } else {
                0
            },
        )
    };

    let started = js_sys::Date::now();
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[android-boot] Extracting boot image header v{version} ({:.1} MiB).",
        input.len() as f64 / 1_048_576.0
    )));
    let files = Array::new();
    let mut total_size = 0usize;
    let mut offset = page_size;
    append_boot_component(
        &files,
        input,
        "kernel",
        offset,
        kernel_size,
        &mut total_size,
    )?;
    offset = align_boot_offset(
        offset
            .checked_add(kernel_size)
            .ok_or_else(|| JsValue::from_str("Android kernel offset overflow."))?,
        page_size,
    )?;
    append_boot_component(
        &files,
        input,
        "ramdisk",
        offset,
        ramdisk_size,
        &mut total_size,
    )?;
    offset = align_boot_offset(
        offset
            .checked_add(ramdisk_size)
            .ok_or_else(|| JsValue::from_str("Android ramdisk offset overflow."))?,
        page_size,
    )?;

    if second_size > 0 {
        append_boot_component(
            &files,
            input,
            "second",
            offset,
            second_size,
            &mut total_size,
        )?;
        offset = align_boot_offset(
            offset
                .checked_add(second_size)
                .ok_or_else(|| JsValue::from_str("Android second-stage offset overflow."))?,
            page_size,
        )?;
    }
    if recovery_size > 0 {
        append_boot_component(
            &files,
            input,
            "recovery_dtbo",
            recovery_offset,
            recovery_size,
            &mut total_size,
        )?;
        offset = align_boot_offset(
            offset
                .checked_add(recovery_size)
                .ok_or_else(|| JsValue::from_str("Android recovery DTBO offset overflow."))?,
            page_size,
        )?;
    }
    if dtb_size > 0 {
        append_boot_component(&files, input, "dtb", offset, dtb_size, &mut total_size)?;
        offset = align_boot_offset(
            offset
                .checked_add(dtb_size)
                .ok_or_else(|| JsValue::from_str("Android DTB offset overflow."))?,
            page_size,
        )?;
    }
    if signature_size > 0 {
        append_boot_component(
            &files,
            input,
            "boot_signature",
            offset,
            signature_size,
            &mut total_size,
        )?;
    }
    if files.length() == 0 {
        return Err(JsValue::from_str(
            "Android boot image contains no payload components.",
        ));
    }
    web_sys::console::info_1(&JsValue::from_str(&format!(
        "[android-boot] Extraction complete: {} components, {:.1} MiB in {:.1}s.",
        files.length(),
        total_size as f64 / 1_048_576.0,
        (js_sys::Date::now() - started) / 1000.0
    )));
    Ok(files)
}

fn msi_error(context: &str, error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&format!("{context}: {error}"))
}

fn log_msi(message: &str) {
    web_sys::console::info_1(&JsValue::from_str(message));
}

fn long_name(value: &str) -> &str {
    let target = value.split_once(':').map_or(value, |(_, target)| target);
    target.split_once('|').map_or(target, |(_, long)| long)
}

fn directory_path(
    start: &str,
    parents: &HashMap<String, String>,
    names: &HashMap<String, String>,
) -> Result<String, JsValue> {
    let mut current = start.to_owned();
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    while !current.is_empty() {
        if !seen.insert(current.clone()) {
            return Err(JsValue::from_str(
                "MSI directory table contains a parent cycle.",
            ));
        }
        if seen.len() > 1024 {
            return Err(JsValue::from_str("MSI directory tree is too deep."));
        }
        if let Some(name) = names.get(&current) {
            let name = long_name(name);
            if !name.is_empty() && name != "." {
                parts.push(name.to_owned());
            }
        }
        match parents.get(&current) {
            Some(parent) if !parent.is_empty() => current = parent.clone(),
            _ => break,
        }
    }
    // The highest directory in the chain is the package's install root
    // (usually TARGETDIR/SourceDir), not a folder to add to the output path.
    parts.pop();
    parts.reverse();
    Ok(parts.join("/"))
}

struct CabinetMember {
    name: String,
    size: u32,
    offset: u32,
    folder: u16,
}

fn read_cab_u16(data: &[u8], offset: usize) -> Result<u16, JsValue> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| JsValue::from_str("Invalid CAB file table offset."))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| JsValue::from_str("Truncated CAB file table."))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_cab_u32(data: &[u8], offset: usize) -> Result<u32, JsValue> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| JsValue::from_str("Invalid CAB file table offset."))?;
    let bytes = data
        .get(offset..end)
        .ok_or_else(|| JsValue::from_str("Truncated CAB file table."))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn cab_members(data: &[u8]) -> Result<(usize, Vec<CabinetMember>), JsValue> {
    if data.get(..4) != Some(b"MSCF") || data.len() < 36 {
        return Err(JsValue::from_str(
            "Embedded stream is not a valid CAB file.",
        ));
    }
    let folder_count = read_cab_u16(data, 26)? as usize;
    let file_count = read_cab_u16(data, 28)? as usize;
    let mut cursor = read_cab_u32(data, 16)? as usize;
    let mut members = Vec::with_capacity(file_count);
    for _ in 0..file_count {
        let size = read_cab_u32(data, cursor)?;
        let offset = read_cab_u32(data, cursor + 4)?;
        let folder = read_cab_u16(data, cursor + 8)?;
        let attributes = read_cab_u16(data, cursor + 14)?;
        if folder as usize >= folder_count {
            return Err(JsValue::from_str(
                "CAB cabinet sets that span multiple cabinets are not supported yet.",
            ));
        }
        let name_start = cursor
            .checked_add(16)
            .ok_or_else(|| JsValue::from_str("Invalid CAB file table offset."))?;
        let tail = data
            .get(name_start..)
            .ok_or_else(|| JsValue::from_str("Truncated CAB file table."))?;
        let name_end = tail
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| JsValue::from_str("Unterminated CAB member name."))?;
        let raw_name = &tail[..name_end];
        // MSI payload keys are ASCII; UTF-8 names are also preserved where present.
        let name = if attributes & 0x80 != 0 {
            String::from_utf8_lossy(raw_name).into_owned()
        } else {
            raw_name.iter().map(|byte| char::from(*byte)).collect()
        };
        cursor = name_start + name_end + 1;
        members.push(CabinetMember {
            name,
            size,
            offset,
            folder,
        });
    }
    Ok((folder_count, members))
}

/// Extract files from an MSI package with embedded cabinets.
#[wasm_bindgen]
pub fn extract_msi(input: &[u8]) -> Result<Array, JsValue> {
    use msi::{Package, PackageType, Select};

    let started = js_sys::Date::now();
    log_msi(&format!(
        "[msi] Starting extraction ({:.1} MiB input).",
        input.len() as f64 / 1_048_576.0
    ));
    let cursor = Cursor::new(input.to_vec());
    let mut package =
        Package::open(cursor).map_err(|error| msi_error("Could not read MSI package", error))?;
    if package.package_type() == PackageType::Patch {
        log_msi("[msi] Patch package detected; scanning embedded streams for CAB payloads.");
        let stream_names: Vec<String> = package.streams().collect();
        let files = Array::new();
        let mut total_output = 0usize;
        let mut cabinet_count = 0usize;
        for stream_name in stream_names {
            let stream = package
                .read_stream(&stream_name)
                .map_err(|error| msi_error("Could not read patch stream", error))?;
            let mut bytes = Vec::new();
            stream
                .take((input.len() as u64).saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|error| msi_error("Could not read patch stream", error))?;
            if !bytes.starts_with(b"MSCF") {
                continue;
            }
            let extracted = extract_archive(&bytes, &format!("{stream_name}.cab"))?;
            if extracted.length() == 0 {
                continue;
            }
            cabinet_count += 1;
            for index in 0..extracted.length() {
                let record = extracted.get(index);
                let name = Reflect::get(&record, &"name".into())?
                    .as_string()
                    .unwrap_or_else(|| "unnamed-file".to_owned());
                let data = Reflect::get(&record, &"data".into())?;
                let data = Uint8Array::new(&data);
                let size = data.length() as usize;
                total_output = total_output.checked_add(size).ok_or_else(|| {
                    JsValue::from_str("MSP extracted contents exceed the 1 GiB browser limit.")
                })?;
                if total_output > max_decompressed_size() {
                    return Err(JsValue::from_str(
                        "MSP extracted contents exceed the 1 GiB browser limit.",
                    ));
                }
                let output_name = format!("{stream_name}/{name}");
                let output_record = Object::new();
                Reflect::set(
                    &output_record,
                    &"name".into(),
                    &JsValue::from_str(&output_name),
                )?;
                Reflect::set(
                    &output_record,
                    &"size".into(),
                    &JsValue::from_f64(size as f64),
                )?;
                Reflect::set(&output_record, &"data".into(), &data)?;
                files.push(&output_record);
            }
        }
        if cabinet_count == 0 {
            return Err(JsValue::from_str(
                "This MSP contains no directly extractable CAB payloads. Patch transforms and binary deltas are not reconstructed.",
            ));
        }
        log_msi(&format!(
            "[msi] Extracted {} file(s) from {cabinet_count} embedded patch cabinet(s).",
            files.length()
        ));
        return Ok(files);
    }
    for table in ["File", "Component", "Directory", "Media"] {
        if !package.has_table(table) {
            return Err(JsValue::from_str(&format!(
                "MSI package is missing the {table} table."
            )));
        }
    }

    let mut component_dirs = HashMap::new();
    for row in package
        .select_rows(Select::table("Component"))
        .map_err(|error| msi_error("Could not read MSI Component table", error))?
    {
        if let (Some(component), Some(directory)) =
            (row["Component"].as_str(), row["Directory_"].as_str())
        {
            component_dirs.insert(component.to_owned(), directory.to_owned());
        }
    }

    let mut directory_parents = HashMap::new();
    let mut directory_names = HashMap::new();
    for row in package
        .select_rows(Select::table("Directory"))
        .map_err(|error| msi_error("Could not read MSI Directory table", error))?
    {
        if let Some(directory) = row["Directory"].as_str() {
            let name = row["DefaultDir"].as_str().unwrap_or("").to_owned();
            directory_names.insert(directory.to_owned(), name);
            if let Some(parent) = row["Directory_Parent"].as_str() {
                directory_parents.insert(directory.to_owned(), parent.to_owned());
            }
        }
    }

    let mut output_paths = HashMap::new();
    for row in package
        .select_rows(Select::table("File"))
        .map_err(|error| msi_error("Could not read MSI File table", error))?
    {
        let (Some(file_id), Some(file_name), Some(component)) = (
            row["File"].as_str(),
            row["FileName"].as_str(),
            row["Component_"].as_str(),
        ) else {
            continue;
        };
        let directory = component_dirs.get(component).ok_or_else(|| {
            JsValue::from_str(&format!(
                "MSI file {file_id} refers to a missing component."
            ))
        })?;
        let directory = directory_path(directory, &directory_parents, &directory_names)?;
        let filename = long_name(file_name);
        let output_name = if directory.is_empty() {
            filename.to_owned()
        } else {
            format!("{directory}/{filename}")
        };
        output_paths.insert(file_id.to_ascii_lowercase(), output_name);
    }

    let mut media = Vec::new();
    for row in package
        .select_rows(Select::table("Media"))
        .map_err(|error| msi_error("Could not read MSI Media table", error))?
    {
        if let Some(cabinet) = row["Cabinet"].as_str() {
            media.push(cabinet.to_owned());
        }
    }
    log_msi(&format!(
        "[msi] Database parsed: {} payload files, {} media records.",
        output_paths.len(),
        media.len()
    ));

    let files = Array::new();
    let mut total_output = 0usize;
    let mut found_files = HashSet::new();
    let mut embedded_cabinets = 0usize;
    for cabinet_ref in media {
        let Some(stream_name) = cabinet_ref.strip_prefix('#') else {
            continue; // External CAB files are not part of the selected MSI file.
        };
        let stream_name = package
            .streams()
            .find(|candidate| candidate.eq_ignore_ascii_case(stream_name))
            .ok_or_else(|| {
                JsValue::from_str(&format!(
                    "MSI references embedded cabinet “{stream_name}”, but its stream is missing."
                ))
            })?;
        let stream = package
            .read_stream(&stream_name)
            .map_err(|error| msi_error("Could not open embedded MSI cabinet", error))?;
        let mut cab_bytes = Vec::new();
        stream
            .take((input.len() as u64).saturating_add(1))
            .read_to_end(&mut cab_bytes)
            .map_err(|error| msi_error("Could not read embedded MSI cabinet", error))?;
        let (folder_count, members) = cab_members(&cab_bytes)?;
        if members.is_empty() {
            continue;
        }
        embedded_cabinets += 1;
        let mut folder_ends = vec![0u32; folder_count];
        for member in &members {
            let end = member
                .offset
                .checked_add(member.size)
                .ok_or_else(|| JsValue::from_str("CAB member size overflow."))?;
            folder_ends[member.folder as usize] = folder_ends[member.folder as usize].max(end);
        }

        log_msi(&format!(
            "[msi] Cabinet {stream_name}: {} member(s), {folder_count} folder(s).",
            members.len()
        ));
        for (folder_index, folder_end) in folder_ends.iter().enumerate() {
            if *folder_end == 0 {
                continue;
            }
            let has_matching_file = members.iter().any(|member| {
                member.folder as usize == folder_index
                    && output_paths.contains_key(&member.name.to_ascii_lowercase())
            });
            if !has_matching_file {
                continue;
            }
            let remaining = max_decompressed_size().saturating_sub(total_output);
            if *folder_end as usize > remaining {
                return Err(JsValue::from_str(
                    "MSI extracted contents exceed the 1 GiB browser limit.",
                ));
            }

            let folder_started = js_sys::Date::now();
            log_msi(&format!(
                "[msi] Decoding cabinet {stream_name}, folder {}/{folder_count} ({:.1} MiB) once.",
                folder_index + 1,
                *folder_end as f64 / 1_048_576.0
            ));
            // CAB files in one folder share a solid compression stream. Calling
            // cab::Cabinet::read_file for every member replays that stream from
            // its beginning each time. Make a temporary CAB view with one entry
            // spanning the whole folder so the codec decodes it only once.
            let mut synthetic = cab_bytes.clone();
            let file_table = read_cab_u32(&synthetic, 16)? as usize;
            synthetic[28..30].copy_from_slice(&1u16.to_le_bytes());
            synthetic[file_table..file_table + 4].copy_from_slice(&folder_end.to_le_bytes());
            synthetic[file_table + 4..file_table + 8].copy_from_slice(&0u32.to_le_bytes());
            synthetic[file_table + 8..file_table + 10]
                .copy_from_slice(&(folder_index as u16).to_le_bytes());
            let mut cabinet = cab::Cabinet::new(Cursor::new(synthetic))
                .map_err(|error| msi_error("Could not prepare embedded CAB folder", error))?;
            let first_member = &members[0].name;
            let reader = cabinet
                .read_file(first_member)
                .map_err(|error| msi_error("Could not open embedded CAB folder", error))?;
            let mut folder_data = Vec::with_capacity(*folder_end as usize);
            reader
                .take(*folder_end as u64)
                .read_to_end(&mut folder_data)
                .map_err(|error| msi_error("Could not decompress embedded CAB folder", error))?;
            if folder_data.len() != *folder_end as usize {
                return Err(JsValue::from_str(
                    "Embedded CAB folder ended before all declared files were read.",
                ));
            }

            for member in members
                .iter()
                .filter(|member| member.folder as usize == folder_index)
            {
                let key = member.name.to_ascii_lowercase();
                let Some(output_name) = output_paths.get(&key) else {
                    continue;
                };
                let start = member.offset as usize;
                let end = start
                    .checked_add(member.size as usize)
                    .ok_or_else(|| JsValue::from_str("CAB member range overflow."))?;
                let data = folder_data.get(start..end).ok_or_else(|| {
                    JsValue::from_str("CAB member points outside its decompressed folder.")
                })?;
                files.push(&make_record(output_name, data)?);
                total_output += data.len();
                found_files.insert(key);
            }
            log_msi(&format!(
                "[msi] Finished folder {}/{} in {:.1}s; {} MiB returned so far.",
                folder_index + 1,
                folder_count,
                (js_sys::Date::now() - folder_started) / 1000.0,
                total_output / 1_048_576
            ));
        }
    }

    if embedded_cabinets == 0 {
        return Err(JsValue::from_str(
            "No embedded MSI cabinets were found. This package may use external cabinet files, which must be selected separately.",
        ));
    }
    if files.length() == 0 {
        return Err(JsValue::from_str(
            "No files from the MSI File table were found in its embedded cabinets.",
        ));
    }
    let unavailable_files = output_paths.len().saturating_sub(found_files.len());
    if unavailable_files != 0 {
        return Err(JsValue::from_str(&format!(
            "MSI lists {unavailable_files} file(s) that are not in its embedded cabinets. This package may need external CAB files or use an unsupported loose-file layout."
        )));
    }
    log_msi(&format!(
        "[msi] Extraction complete: {} files, {:.1} MiB in {:.1}s.",
        files.length(),
        total_output as f64 / 1_048_576.0,
        (js_sys::Date::now() - started) / 1000.0
    ));
    Ok(files)
}
