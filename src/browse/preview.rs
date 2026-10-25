//! Turning one blob into something the preview panel can draw.
//!
//! Runs on the worker thread: image decoding (including HEIC), plist
//! formatting, SQLite table reads and hex dumps all happen here so the UI
//! thread never blocks on disk or CPU.

use std::io::Cursor;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use image::ImageDecoder;

use super::model::{Backup, Entry, Kind, MAX_PREVIEW_BYTES, MAX_TEXT_BYTES};

/// What the preview panel shows for the selected entry.
pub enum Preview {
    /// The row exists in Manifest.db but its blob is not in this backup
    /// (normal for incremental backups: unchanged files are not copied again).
    Missing,
    Image(ImagePreview),
    Text { text: String, note: String },
    Plist(String),
    Sqlite { tables: Vec<String> },
    Hex { text: String, note: String },
    Media { kind: Kind, note: String },
    Error(String),
}

pub struct ImagePreview {
    pub width: usize,
    pub height: usize,
    /// Tightly packed RGBA, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    pub note: String,
}

impl ImagePreview {
    pub fn egui_image(&self) -> eframe::egui::ColorImage {
        eframe::egui::ColorImage::from_rgba_unmultiplied([self.width, self.height], &self.rgba)
    }
}

/// Build the preview for `entry`. Never returns Err: failures become
/// [`Preview::Error`] with a readable message.
pub fn load(backup: &Backup, entry: &Entry) -> Preview {
    if entry.is_link() {
        return Preview::Error(
            "This entry is a symbolic link inside the backup, not a file with contents.".into(),
        );
    }
    if !backup.blob_exists(entry) {
        return Preview::Missing;
    }

    let kind = entry.kind();
    if matches!(kind, Kind::Video | Kind::Audio) {
        return Preview::Media {
            kind,
            note: if kind == Kind::Video {
                "Videos are not rendered in the window. Use “Open in system viewer” to play \
                 the extracted copy."
                    .into()
            } else {
                "Audio is not played in the window. Use “Open in system viewer” to play the \
                 extracted copy."
                    .into()
            },
        };
    }

    let head = backup.read_blob_prefix(entry, 64).unwrap_or_default();
    let ext = entry.extension();
    let note_size = entry.size;

    // SQLite: extension or the on-disk magic header.
    let sqlite_ext = matches!(
        ext.to_ascii_lowercase().as_str(),
        "db" | "sqlite" | "sqlite3" | "sqlitedb" | "db2"
    );
    if sqlite_ext || head.starts_with(b"SQLite format 3\0") {
        let path = backup.blob_path(entry);
        return match tables(&path) {
            Ok(tables) if !tables.is_empty() => Preview::Sqlite { tables },
            Ok(_) => Preview::Error("This SQLite database has no tables.".into()),
            Err(err) => Preview::Error(format!("Could not read the SQLite database: {err:#}")),
        };
    }

    // Property lists: binary (`bplist00`) or XML, regardless of extension.
    let plist_like = ext.eq_ignore_ascii_case("plist")
        || head.starts_with(b"bplist00")
        || head.windows(6).any(|w| w == b"<plist");
    if plist_like {
        let cap = MAX_PREVIEW_BYTES.min(16 * 1024 * 1024);
        match backup.read_blob_prefix(entry, cap) {
            Ok(data) => match pretty_plist(&data) {
                Ok(xml) => return Preview::Plist(xml),
                Err(err) => {
                    // A broken plist is still worth showing as text/hex.
                    if let Some(text) = sniff_text(&data) {
                        return Preview::Text {
                            text,
                            note: format!("Not a valid property list ({err:#}); showing raw text."),
                        };
                    }
                    return hex_preview(&data, note_size, cap);
                }
            },
            Err(err) => return Preview::Error(format!("Could not read the file: {err:#}")),
        }
    }

    if kind == Kind::Image || looks_heic(&head) {
        let cap = MAX_PREVIEW_BYTES;
        let data = match backup.read_blob_prefix(entry, cap) {
            Ok(d) => d,
            Err(err) => return Preview::Error(format!("Could not read the file: {err:#}")),
        };
        let truncated = (data.len() as u64) < entry.size;
        return match decode_image(&data, ext, truncated) {
            Ok(p) => Preview::Image(p),
            Err(err) => Preview::Error(format!(
                "Could not decode this image: {err:#}\n\nUse “Open in system viewer” to open \
                 the extracted copy."
            )),
        };
    }

    let text_like = matches!(kind, Kind::Text) || sniff_text(&head).is_some();
    if text_like {
        let cap = MAX_TEXT_BYTES;
        return match backup.read_blob_prefix(entry, cap) {
            Ok(data) => {
                let text = decode_text(&data);
                let mut note = String::new();
                if data.len() as u64 >= cap && entry.size > cap {
                    note = format!(
                        "Showing the first {} of {}.",
                        crate::util::human_bytes(cap),
                        crate::util::human_bytes(entry.size)
                    );
                }
                Preview::Text { text, note }
            }
            Err(err) => Preview::Error(format!("Could not read the file: {err:#}")),
        };
    }

    let cap = 16 * 1024;
    match backup.read_blob_prefix(entry, cap) {
        Ok(data) => hex_preview(&data, note_size, cap),
        Err(err) => Preview::Error(format!("Could not read the file: {err:#}")),
    }
}

fn hex_preview(data: &[u8], size: u64, cap: u64) -> Preview {
    let note = if size > cap {
        format!(
            "Binary file — showing the first {} of {}.",
            crate::util::human_bytes(cap),
            crate::util::human_bytes(size)
        )
    } else {
        "Binary file (no preview for this format).".to_string()
    };
    Preview::Hex {
        text: hex_dump(data),
        note,
    }
}

/// Hex + ASCII dump, 16 bytes per row.
pub fn hex_dump(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 4);
    for (offset, chunk) in data.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", offset * 16));
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => out.push_str(&format!("{b:02x} ")),
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        for b in chunk {
            out.push(if (0x20..0x7f).contains(b) { *b as char } else { '.' });
        }
        out.push('\n');
    }
    if data.is_empty() {
        out.push_str("(empty file)\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Property lists
// ---------------------------------------------------------------------------

pub fn pretty_plist(data: &[u8]) -> Result<String> {
    let value = plist::Value::from_reader(Cursor::new(data))
        .context("unrecognised property list format")?;
    let mut xml = Vec::new();
    value
        .to_writer_xml(&mut xml)
        .context("could not format property list")?;
    Ok(String::from_utf8_lossy(&xml).into_owned())
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Decode UTF-8 (with or without BOM) or UTF-16 (BOM) text, lossily.
pub fn decode_text(data: &[u8]) -> String {
    if let Some(rest) = data.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = data.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = data.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(data).into_owned()
}

/// Return text when `data` looks like human-readable UTF-8: valid UTF-8 with
/// very few control characters (newlines/tabs allowed).
pub fn sniff_text(data: &[u8]) -> Option<String> {
    if data.is_empty() || data.contains(&0) {
        return None;
    }
    let text = decode_text(data);
    let mut control = 0usize;
    let mut total = 0usize;
    for c in text.chars() {
        total += 1;
        if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
            control += 1;
        }
    }
    if total == 0 || control * 10 > total {
        return None;
    }
    Some(text)
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

pub fn looks_heic(data: &[u8]) -> bool {
    if data.len() < 12 || &data[4..8] != b"ftyp" {
        return false;
    }
    matches!(
        &data[8..12],
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"msf1"
    )
}

fn decode_image(data: &[u8], ext: &str, truncated: bool) -> Result<ImagePreview> {
    if truncated {
        bail!("the file is larger than the preview limit and was only partially read");
    }
    if looks_heic(data) {
        decode_heic(data)
    } else {
        let reader = image::ImageReader::new(Cursor::new(data))
            .with_guessed_format()
            .context("unknown image format")?;
        let format = reader
            .format()
            .map(|f| format!("{f:?}"))
            .unwrap_or_else(|| ext.to_ascii_uppercase());
        let mut decoder = reader.into_decoder().context("unsupported image data")?;
        let orientation = decoder
            .orientation()
            .unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut img = image::DynamicImage::from_decoder(decoder)
            .context("the image data is incomplete or corrupt")?;
        img.apply_orientation(orientation);
        let (w, h) = (img.width() as usize, img.height() as usize);
        let img = downscale(img, 1600);
        let rgba = img.to_rgba8();
        let (pw, ph) = (rgba.width() as usize, rgba.height() as usize);
        Ok(ImagePreview {
            width: pw,
            height: ph,
            rgba: rgba.into_raw(),
            note: if (pw, ph) != (w, h) {
                format!("{format} · preview downscaled from {w}×{h}")
            } else {
                format!("{format} · {w}×{h}")
            },
        })
    }
}

fn downscale(img: image::DynamicImage, max: u32) -> image::DynamicImage {
    if img.width() <= max && img.height() <= max {
        return img;
    }
    let scale = max as f32 / img.width().max(img.height()) as f32;
    let w = ((img.width() as f32 * scale).round() as u32).max(1);
    let h = ((img.height() as f32 * scale).round() as u32).max(1);
    img.thumbnail(w, h)
}

fn decode_heic(data: &[u8]) -> Result<ImagePreview> {
    use zune_heic::zune_core::colorspace::ColorSpace;
    use zune_heic::zune_core::bytestream::ZCursor;
    use zune_heic::HeifDecoder;

    let mut decoder = HeifDecoder::new(ZCursor::new(data));
    let pixels = decoder
        .decode()
        .map_err(|e| anyhow!("{e:?}"))
        .context("HEIC/HEIF decode failed")?;
    let w = decoder.width().context("HEIC image has no width")?;
    let h = decoder.height().context("HEIC image has no height")?;
    let colorspace = decoder.colorspace().unwrap_or(ColorSpace::RGB);
    if pixels.len() < w * h * colorspace.num_components() {
        bail!("HEIC pixel buffer is shorter than expected");
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    match colorspace {
        ColorSpace::RGBA => rgba.extend_from_slice(&pixels[..w * h * 4]),
        ColorSpace::RGB => {
            for px in pixels.chunks_exact(3).take(w * h) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        ColorSpace::Luma => {
            for &v in pixels.iter().take(w * h) {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        ColorSpace::LumaA => {
            for px in pixels.chunks_exact(2).take(w * h) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
        }
        other => bail!("unsupported HEIC colour space {other:?}"),
    }
    let (pw, ph) = fit_within(w, h, 1600);
    let (rgba, pw, ph) = if (pw, ph) != (w, h) {
        let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
            .context("HEIC pixel buffer size mismatch")?;
        let img = image::DynamicImage::ImageRgba8(img).thumbnail(pw as u32, ph as u32);
        let out = img.to_rgba8();
        (out.as_raw().clone(), out.width() as usize, out.height() as usize)
    } else {
        (rgba, w, h)
    };
    Ok(ImagePreview {
        width: pw,
        height: ph,
        rgba,
        note: if (pw, ph) != (w, h) {
            format!("HEIC · preview downscaled from {w}×{h}")
        } else {
            format!("HEIC · {w}×{h}")
        },
    })
}

fn fit_within(w: usize, h: usize, max: usize) -> (usize, usize) {
    if w <= max && h <= max {
        return (w, h);
    }
    let scale = max as f64 / w.max(h) as f64;
    (
        ((w as f64 * scale).round() as usize).max(1),
        ((h as f64 * scale).round() as usize).max(1),
    )
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

pub fn tables(path: &Path) -> Result<Vec<String>> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("could not open {}", path.display()))?;
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view') ORDER BY name")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub struct TableData {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub truncated: bool,
}

/// Read up to 200 rows of one table, rendering values as short strings.
pub fn load_table(path: &Path, table: &str) -> Result<TableData> {
    const LIMIT: usize = 200;
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("could not open {}", path.display()))?;
    let quoted = table.replace('"', "\"\"");
    let sql = format!("SELECT * FROM \"{quoted}\" LIMIT {}", LIMIT + 1);
    let mut stmt = conn.prepare(&sql).context("could not read the table")?;
    let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut result = TableData {
        columns,
        rows: Vec::new(),
        truncated: false,
    };
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if result.rows.len() == LIMIT {
            result.truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(result.columns.len());
        for i in 0..result.columns.len() {
            let cell = match row.get_ref(i) {
                Ok(rusqlite::types::ValueRef::Null) => "NULL".to_string(),
                Ok(rusqlite::types::ValueRef::Integer(v)) => v.to_string(),
                Ok(rusqlite::types::ValueRef::Real(v)) => v.to_string(),
                Ok(rusqlite::types::ValueRef::Text(v)) => {
                    let mut s = String::from_utf8_lossy(v).replace(['\n', '\r', '\t'], " ");
                    if s.chars().count() > 120 {
                        s = s.chars().take(117).collect::<String>() + "…";
                    }
                    s
                }
                Ok(rusqlite::types::ValueRef::Blob(v)) => format!("<blob {} bytes>", v.len()),
                Err(_) => "?".to_string(),
            };
            cells.push(cell);
        }
        result.rows.push(cells);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nope(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ibackup-preview-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn png_round_trip() {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let preview = decode_image(&png, "png", false).unwrap();
        assert_eq!((preview.width, preview.height), (4, 4));
        assert_eq!(&preview.rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn text_and_hex() {
        assert!(sniff_text(b"hello\nworld").is_some());
        assert!(sniff_text(&[0, 1, 2, 3]).is_none());
        assert!(hex_dump(b"ABC").starts_with("00000000  41 42 43"));
        assert_eq!(decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
    }

    #[test]
    fn plist_is_formatted() {
        let mut dict = plist::Dictionary::new();
        dict.insert("Answer".into(), 42u64.into());
        let mut bin = Vec::new();
        plist::Value::Dictionary(dict)
            .to_writer_binary(&mut bin)
            .unwrap();
        let xml = pretty_plist(&bin).unwrap();
        assert!(xml.contains("<key>Answer</key>"));
        assert!(xml.contains("<integer>42</integer>"));
    }

    #[test]
    fn sqlite_tables_and_rows() {
        let path = nope("sqlite");
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, data BLOB);
                 INSERT INTO people (name, data) VALUES ('Ada', x'0102'), ('Grace', NULL);",
            )
            .unwrap();
        }
        assert_eq!(tables(&path).unwrap(), vec!["people"]);
        let data = load_table(&path, "people").unwrap();
        assert_eq!(data.columns, vec!["id", "name", "data"]);
        assert_eq!(data.rows.len(), 2);
        assert_eq!(data.rows[0][1], "Ada");
        assert_eq!(data.rows[0][2], "<blob 2 bytes>");
        assert_eq!(data.rows[1][2], "NULL");
        assert!(!data.truncated);
        std::fs::remove_file(&path).ok();
    }
}
