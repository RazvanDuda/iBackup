//! Read-only model of an iOS backup for the explorer window.
//!
//! A backup folder holds `Manifest.plist` (device metadata) and `Manifest.db`
//! (one row per file, with the per-file metadata as a nested binary plist),
//! plus content-addressed blobs in `<first two hex chars>/<fileID>`.
//!
//! Everything here is independent of egui so it can be unit tested: resolving
//! the folder the user dropped, indexing `Manifest.db` into an in-memory tree,
//! listing/searching that tree, and extracting blobs back to normal files.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};

use crate::util;

/// A preview never reads more than this from one blob (keeps huge videos and
/// SQLite files from stalling the worker).
pub const MAX_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;
/// Text-ish previews stop here.
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// One file, folder or symlink from `Manifest.db`.
#[derive(Clone)]
pub struct Entry {
    pub file_id: Box<str>,
    pub domain: u32,
    pub path: Box<str>,
    /// 1 = file, 2 = directory, 4 = symlink (the `flags` column).
    pub flags: u8,
    pub size: u64,
    /// Unix seconds from the nested plist's `LastModified` (0 when unknown).
    pub modified: i64,
    /// Directory invented because the backup only lists files (incremental
    /// backups can omit an ancestor folder row).
    pub synthetic: bool,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags == 2
    }

    pub fn is_link(&self) -> bool {
        self.flags == 4
    }

    pub fn is_file(&self) -> bool {
        self.flags == 1
    }

    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    pub fn parent(&self) -> Option<&str> {
        parent_of(&self.path)
    }

    pub fn extension(&self) -> &str {
        let name = self.name();
        match name.rfind('.') {
            Some(i) if i + 1 < name.len() => &name[i + 1..],
            _ => "",
        }
    }

    pub fn kind(&self) -> Kind {
        if self.is_dir() {
            Kind::Dir
        } else {
            Kind::from_extension(self.extension())
        }
    }
}

fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

/// What the list view shows for a row, driven by the file extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    Image,
    Video,
    Audio,
    Text,
    Plist,
    Database,
    Archive,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Dir => "Folder",
            Kind::Image => "Photo",
            Kind::Video => "Video",
            Kind::Audio => "Audio",
            Kind::Text => "Text",
            Kind::Plist => "Plist",
            Kind::Database => "Database",
            Kind::Archive => "Archive",
            Kind::Other => "File",
        }
    }

    fn from_extension(ext: &str) -> Kind {
        if ext.is_empty() {
            return Kind::Other;
        }
        const IMAGES: &[&str] = &[
            "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "tif", "tiff", "bmp", "dng",
            "cr2", "nef", "arw", "raw", "ico", "avif",
        ];
        const VIDEOS: &[&str] = &["mov", "mp4", "m4v", "avi", "mkv", "3gp", "webm", "mpg", "mpeg"];
        const AUDIO: &[&str] = &["m4a", "mp3", "aac", "wav", "caf", "aiff", "flac", "ogg", "amr"];
        const TEXT: &[&str] = &[
            "txt", "log", "json", "xml", "html", "htm", "css", "js", "md", "csv", "yaml", "yml",
            "ini", "conf", "cfg", "sh", "py", "rs", "toml", "rtf", "vcf", "ics",
        ];
        const ARCHIVES: &[&str] = &["zip", "gz", "tar", "bz2", "xz", "7z", "rar", "ipa"];
        if ext_matches(ext, IMAGES) {
            Kind::Image
        } else if ext_matches(ext, VIDEOS) {
            Kind::Video
        } else if ext_matches(ext, AUDIO) {
            Kind::Audio
        } else if ext.eq_ignore_ascii_case("plist") {
            Kind::Plist
        } else if ext_matches(ext, TEXT) {
            Kind::Text
        } else if ext_matches(ext, &["db", "sqlite", "sqlite3", "db2", "sqlitedb"]) {
            Kind::Database
        } else if ext_matches(ext, ARCHIVES) {
            Kind::Archive
        } else {
            Kind::Other
        }
    }
}

fn ext_matches(ext: &str, list: &[&str]) -> bool {
    list.iter().any(|x| ext.eq_ignore_ascii_case(x))
}

/// The kind filter in the toolbar. Folders are always shown while browsing.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum KindFilter {
    #[default]
    All,
    Images,
    Videos,
    Audio,
    Documents,
    Other,
}

impl KindFilter {
    pub const ALL: [KindFilter; 6] = [
        KindFilter::All,
        KindFilter::Images,
        KindFilter::Videos,
        KindFilter::Audio,
        KindFilter::Documents,
        KindFilter::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            KindFilter::All => "All files",
            KindFilter::Images => "Photos",
            KindFilter::Videos => "Videos",
            KindFilter::Audio => "Audio",
            KindFilter::Documents => "Documents",
            KindFilter::Other => "Other",
        }
    }

    pub fn matches(self, kind: Kind) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::Images => kind == Kind::Image,
            KindFilter::Videos => kind == Kind::Video,
            KindFilter::Audio => kind == Kind::Audio,
            KindFilter::Documents => matches!(kind, Kind::Text | Kind::Plist),
            KindFilter::Other => matches!(
                kind,
                Kind::Database | Kind::Archive | Kind::Other | Kind::Dir
            ),
        }
    }
}

/// Sort order for a listing; folders always come first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Name,
    Size,
    Modified,
}

impl Sort {
    pub const ALL: [Sort; 3] = [Sort::Name, Sort::Size, Sort::Modified];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "Name",
            Sort::Size => "Size",
            Sort::Modified => "Newest",
        }
    }
}

// ---------------------------------------------------------------------------
// Device metadata
// ---------------------------------------------------------------------------

/// The useful subset of `Manifest.plist`.
#[derive(Clone, Default)]
#[allow(dead_code)]
pub struct BackupInfo {
    pub device_name: String,
    pub display_name: String,
    pub product_name: String,
    pub product_type: String,
    pub product_version: String,
    pub build_version: String,
    pub serial: String,
    pub udid: String,
    pub target_id: String,
    pub guid: String,
    pub itunes_version: String,
    pub format_version: u64,
    pub encrypted: bool,
    pub passcode_set: bool,
    /// Display string for "Last Backup Date" in local time.
    pub date: Option<String>,
    /// Unix seconds of the same date.
    pub date_unix: Option<i64>,
}

impl BackupInfo {
    fn read(root: &Path) -> Result<BackupInfo> {
        let manifest = root.join("Manifest.plist");
        let dict: plist::Dictionary = plist::from_file(&manifest)
            .with_context(|| format!("could not read {}", manifest.display()))?;
        let s = |key: &str| {
            dict.get(key)
                .and_then(|v| v.as_string())
                .unwrap_or_default()
                .to_string()
        };
        let b = |key: &str| dict.get(key).and_then(|v| v.as_boolean()).unwrap_or(false);
        let u = |key: &str| {
            dict.get(key)
                .and_then(|v| v.as_unsigned_integer())
                .unwrap_or_default()
        };
        let date = dict.get("Last Backup Date").and_then(|v| v.as_date());
        let date_unix = date.and_then(|d| {
            DateTime::parse_from_rfc3339(&d.to_xml_format())
                .ok()
                .map(|t| t.timestamp())
        });
        Ok(BackupInfo {
            device_name: s("Device Name"),
            display_name: s("Display Name"),
            product_name: s("Product Name"),
            product_type: s("Product Type"),
            product_version: s("Product Version"),
            build_version: s("Build Version"),
            serial: s("Serial Number"),
            udid: s("Unique Identifier"),
            target_id: s("Target Identifier"),
            guid: s("GUID"),
            itunes_version: s("iTunes Version"),
            format_version: u("Version"),
            encrypted: b("IsEncrypted"),
            passcode_set: b("Passcode Set"),
            date: date_unix.and_then(|ts| {
                DateTime::from_timestamp(ts, 0)
                    .map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            }),
            date_unix,
        })
    }

    fn fallback(root: &Path) -> BackupInfo {
        // `.../DeviceName_1A2B3C4D/backup/<UDID>` → "DeviceName"
        let mut name = root.file_name().unwrap_or_default().to_string_lossy().to_string();
        if let Some(parent) = root.parent().and_then(|p| p.parent()) {
            if let Some(folder) = parent.file_name() {
                let folder = folder.to_string_lossy();
                if let Some((device, suffix)) = folder.rsplit_once('_') {
                    if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
                        name = device.to_string();
                    }
                }
            }
        }
        BackupInfo {
            device_name: name,
            ..BackupInfo::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Domains
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Domain {
    pub name: Box<str>,
    pub files: usize,
    pub dirs: usize,
    pub links: usize,
    pub size: u64,
}

/// A failed open, with whatever metadata could still be read (so the window
/// can show the device card even when the backup is encrypted).
#[derive(Clone)]
pub struct OpenError {
    pub message: String,
    pub info: Option<BackupInfo>,
    pub dir: Option<PathBuf>,
}

impl std::fmt::Debug for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenError")
            .field("message", &self.message)
            .field("dir", &self.dir)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The indexed backup
// ---------------------------------------------------------------------------

pub struct Backup {
    pub dir: PathBuf,
    pub info: BackupInfo,
    pub entries: Vec<Entry>,
    pub domains: Vec<Domain>,
    pub total_size: u64,
    pub file_count: usize,
    pub dir_count: usize,
    pub link_count: usize,
    /// domain → parent folder → child entry indices.
    children: HashMap<u32, HashMap<Box<str>, Vec<u32>>>,
}

impl Backup {
    /// Resolve whatever the user picked and index it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(input: &Path) -> std::result::Result<Backup, OpenError> {
        Self::open_with_progress(input, &mut |_| {})
    }

    /// Like [`Backup::open`], reporting the number of rows indexed so far.
    pub fn open_with_progress(
        input: &Path,
        progress: &mut dyn FnMut(usize),
    ) -> std::result::Result<Backup, OpenError> {
        let dir = match resolve_root(input) {
            Ok(d) => d,
            Err(e) => {
                return Err(OpenError {
                    message: format!("{e:#}"),
                    info: None,
                    dir: None,
                })
            }
        };
        let info = BackupInfo::read(&dir).unwrap_or_else(|_| BackupInfo::fallback(&dir));
        match Self::load(dir.clone(), info, progress) {
            Ok(b) => Ok(b),
            Err(e) => Err(OpenError {
                message: format!("{e:#}"),
                info: Some(BackupInfo::read(&dir).unwrap_or_else(|_| BackupInfo::fallback(&dir))),
                dir: Some(dir),
            }),
        }
    }

    fn load(
        dir: PathBuf,
        info: BackupInfo,
        progress: &mut dyn FnMut(usize),
    ) -> Result<Backup> {
        let manifest = dir.join("Manifest.plist");
        if !manifest.is_file() {
            bail!(
                "{} does not contain Manifest.plist, so it is not an iOS backup folder.",
                dir.display()
            );
        }
        if info.encrypted {
            bail!(
                "This backup is encrypted, so its contents cannot be browsed directly.\n\n\
                 Turn off \"Encrypt local backup\" for this iPhone (or run \
                 `ibackup encryption disable` while it is connected), then make a new backup. \
                 Encrypted backups are not supported by the explorer yet."
            );
        }
        let db = dir.join("Manifest.db");
        if !db.is_file() {
            bail!(
                "{} does not contain Manifest.db, so its file list is missing.",
                dir.display()
            );
        }

        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .with_context(|| format!("could not open {}", db.display()))?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(10));

        let mut entries: Vec<Entry> = Vec::new();
        let mut domains: Vec<Domain> = Vec::new();
        let mut domain_ids: HashMap<Box<str>, u32> = HashMap::new();
        let mut next_domain = 0u32;

        {
            let mut stmt = conn
                .prepare("SELECT fileID, domain, relativePath, flags, file FROM Files")
                .context("could not read the Files table of Manifest.db")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let file_id: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
                if file_id.is_empty() {
                    continue;
                }
                let domain_name: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                let path: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
                let flags: i64 = row.get::<_, Option<i64>>(3)?.unwrap_or(1);
                let blob: Option<Vec<u8>> = row.get(4)?;

                let mut size = 0u64;
                let mut modified = 0i64;
                if let Some(bytes) = blob {
                    if let Ok(value) = plist::Value::from_reader(std::io::Cursor::new(&bytes)) {
                        if let Some(dict) = value.as_dictionary() {
                            if let Some(s) = dict.get("Size").and_then(|v| v.as_unsigned_integer()) {
                                size = s;
                            }
                            if let Some(m) = dict
                                .get("LastModified")
                                .and_then(|v| v.as_unsigned_integer())
                            {
                                modified = m as i64;
                            }
                        }
                    }
                }

                let domain = *domain_ids.entry(domain_name.as_str().into()).or_insert_with(|| {
                    let id = next_domain;
                    next_domain += 1;
                    domains.push(Domain {
                        name: domain_name.clone().into_boxed_str(),
                        files: 0,
                        dirs: 0,
                        links: 0,
                        size: 0,
                    });
                    id
                });

                let flags = match flags {
                    2 => 2,
                    4 => 4,
                    _ => 1,
                };
                entries.push(Entry {
                    file_id: file_id.into_boxed_str(),
                    domain,
                    path: path.into_boxed_str(),
                    flags,
                    size,
                    modified,
                    synthetic: false,
                });

                if entries.len() % 4096 == 0 {
                    progress(entries.len());
                }
            }
        }
        progress(entries.len());

        let children = Self::build_tree(&mut entries);
        let (file_count, dir_count, link_count, total_size) = Self::tally(&entries, &mut domains);

        Ok(Backup {
            dir,
            info,
            entries,
            domains,
            total_size,
            file_count,
            dir_count,
            link_count,
            children,
        })
    }

    fn tally(entries: &[Entry], domains: &mut [Domain]) -> (usize, usize, usize, u64) {
        let mut files = 0;
        let mut dirs = 0;
        let mut links = 0;
        let mut bytes = 0u64;
        for d in domains.iter_mut() {
            d.files = 0;
            d.dirs = 0;
            d.links = 0;
            d.size = 0;
        }
        for e in entries {
            let d = &mut domains[e.domain as usize];
            match e.flags {
                2 => {
                    dirs += 1;
                    d.dirs += 1;
                }
                4 => {
                    links += 1;
                    d.links += 1;
                }
                _ => {
                    files += 1;
                    bytes = bytes.saturating_add(e.size);
                    d.files += 1;
                    d.size = d.size.saturating_add(e.size);
                }
            }
        }
        (files, dirs, links, bytes)
    }

    /// Group every entry under its parent folder, inventing missing ancestors.
    fn build_tree(entries: &mut Vec<Entry>) -> HashMap<u32, HashMap<Box<str>, Vec<u32>>> {
        let mut existing: HashSet<(u32, Box<str>)> = entries
            .iter()
            .filter(|e| e.is_dir())
            .map(|e| (e.domain, e.path.clone()))
            .collect();

        // Websites/AppDomain folders are deep; walk each file's ancestor chain
        // from the domain root so intermediate folders appear in the tree.
        let mut created: Vec<(u32, String)> = Vec::new();
        let mut seen: HashSet<(u32, Box<str>)> = HashSet::new();
        for e in entries.iter() {
            if e.is_dir() || e.path.is_empty() {
                continue;
            }
            let mut acc = String::new();
            let mut segments: Vec<&str> = e.path.split('/').collect();
            segments.pop(); // the file itself
            for segment in segments {
                if segment.is_empty() {
                    continue;
                }
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(segment);
                let key = (e.domain, acc.clone().into_boxed_str());
                if !existing.contains(&key) && seen.insert(key) {
                    created.push((e.domain, acc.clone()));
                }
            }
        }
        for (domain, path) in created {
            entries.push(Entry {
                file_id: format!("synthetic:{domain}:{path}").into_boxed_str(),
                domain,
                path: path.into_boxed_str(),
                flags: 2,
                size: 0,
                modified: 0,
                synthetic: true,
            });
            existing.insert((domain, entries.last().unwrap().path.clone()));
        }

        let mut children: HashMap<u32, HashMap<Box<str>, Vec<u32>>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            let parent: Box<str> = e.parent().unwrap_or("").into();
            children
                .entry(e.domain)
                .or_default()
                .entry(parent)
                .or_default()
                .push(i as u32);
        }
        children
    }

    pub fn domain_name(&self, domain: u32) -> &str {
        self.domains
            .get(domain as usize)
            .map(|d| &*d.name)
            .unwrap_or("")
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn domain_id(&self, name: &str) -> Option<u32> {
        self.domains
            .iter()
            .position(|d| &*d.name == name)
            .map(|i| i as u32)
    }

    pub fn blob_path(&self, entry: &Entry) -> PathBuf {
        blob_path(&self.dir, &entry.file_id)
    }

    pub fn blob_exists(&self, entry: &Entry) -> bool {
        self.blob_path(entry).is_file()
    }

    /// Read at most `max` bytes from the start of a blob.
    pub fn read_blob_prefix(&self, entry: &Entry, max: u64) -> std::io::Result<Vec<u8>> {
        let path = self.blob_path(entry);
        let file = File::open(&path)?;
        let mut out = Vec::new();
        file.take(max).read_to_end(&mut out)?;
        Ok(out)
    }

    /// Entries directly inside `dir` (empty string = the domain root).
    pub fn listing(&self, domain: u32, dir: &str, filter: KindFilter, sort: Sort) -> Vec<u32> {
        let Some(folders) = self.children.get(&domain) else {
            return Vec::new();
        };
        let Some(ids) = folders.get(dir) else {
            return Vec::new();
        };
        let mut out: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|&i| {
                let e = &self.entries[i as usize];
                e.is_dir() || filter.matches(e.kind())
            })
            .collect();
        out.sort_by(|&a, &b| cmp_entries(&self.entries[a as usize], &self.entries[b as usize], sort));
        out
    }

    /// Number of entries directly inside `dir` (cheap: one hash lookup).
    pub fn direct_count(&self, domain: u32, dir: &str) -> usize {
        self.children
            .get(&domain)
            .and_then(|m| m.get(dir))
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Number of files and folders below `dir` (for folder rows and previews).
    pub fn folder_counts(&self, domain: u32, dir: &str) -> (usize, usize) {
        let mut files = 0;
        let mut folders = 0;
        if let Some(map) = self.children.get(&domain) {
            for (parent, ids) in map {
                let under = if dir.is_empty() {
                    true
                } else {
                    parent.as_ref() == dir
                        || (parent.len() > dir.len()
                            && parent.starts_with(dir)
                            && parent.as_bytes().get(dir.len()) == Some(&b'/'))
                };
                if !under {
                    continue;
                }
                for &i in ids {
                    let e = &self.entries[i as usize];
                    if e.is_dir() {
                        if &*e.path != dir {
                            folders += 1;
                        }
                    } else {
                        files += 1;
                    }
                }
            }
        }
        (files, folders)
    }

    /// Total file count and bytes below `dir` (used by "extract folder").
    pub fn folder_totals(&self, domain: u32, dir: &str) -> (usize, u64) {
        let mut files = 0;
        let mut bytes = 0u64;
        for e in &self.entries {
            if e.domain == domain && e.is_file() && under_dir(&e.path, dir) {
                files += 1;
                bytes = bytes.saturating_add(e.size);
            }
        }
        (files, bytes)
    }

    pub fn files_under(&self, domain: u32, dir: &str) -> Vec<u32> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.domain == domain && e.is_file() && under_dir(&e.path, dir))
            .map(|(i, _)| i as u32)
            .collect()
    }

    /// Case-insensitive substring search over names (ranked first) and paths.
    pub fn search(
        &self,
        query: &str,
        domain: Option<u32>,
        filter: KindFilter,
        limit: usize,
    ) -> (Vec<u32>, bool) {
        let query = query.trim();
        if query.is_empty() {
            return (Vec::new(), false);
        }
        let needle_lower = if query.is_ascii() {
            String::new()
        } else {
            query.to_lowercase()
        };
        let needle = if query.is_ascii() {
            query
        } else {
            &needle_lower
        };

        let mut names = Vec::new();
        let mut paths = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            if let Some(d) = domain {
                if e.domain != d {
                    continue;
                }
            }
            if e.is_dir() {
                if filter != KindFilter::All {
                    continue;
                }
            } else if !filter.matches(e.kind()) {
                continue;
            }
            if ci_contains(e.name(), needle) {
                names.push(i as u32);
            } else if ci_contains(&e.path, needle) {
                paths.push(i as u32);
            }
            if names.len() + paths.len() >= 5000 {
                break;
            }
        }
        let sort = Sort::Name;
        names.sort_by(|&a, &b| cmp_entries(&self.entries[a as usize], &self.entries[b as usize], sort));
        paths.sort_by(|&a, &b| cmp_entries(&self.entries[a as usize], &self.entries[b as usize], sort));
        names.extend(paths);
        let truncated = names.len() > limit;
        names.truncate(limit);
        (names, truncated)
    }

    /// Copy the blobs for `indices` into `out`, recreating `<Domain>/<path>`.
    pub fn extract(
        &self,
        indices: &[u32],
        out: &Path,
        progress: &mut dyn FnMut(usize, usize, u64),
    ) -> ExtractOutcome {
        let mut result = ExtractOutcome {
            files: 0,
            bytes: 0,
            skipped: 0,
            missing: 0,
            errors: Vec::new(),
            out: out.to_path_buf(),
        };
        let total = indices.len();
        for (done, &i) in indices.iter().enumerate() {
            if let Some(e) = self.entries.get(i as usize) {
                if !e.is_file() {
                    result.skipped += 1;
                    continue;
                }
                let source = self.blob_path(e);
                if !source.is_file() {
                    result.missing += 1;
                    continue;
                }
                let rel = format!("{}/{}", self.domain_name(e.domain), e.path);
                let dest = util::local_path_for(out, &rel);
                let copied = dest
                    .parent()
                    .map(fs::create_dir_all)
                    .transpose()
                    .and_then(|_| fs::copy(&source, &dest));
                match copied {
                    Ok(bytes) => {
                        result.files += 1;
                        result.bytes = result.bytes.saturating_add(bytes);
                    }
                    Err(err) => {
                        if result.errors.len() < 200 {
                            result.errors.push(format!("{}: {err:#}", e.path));
                        }
                    }
                }
            }
            progress(done + 1, total, result.bytes);
        }
        result
    }
}

pub struct ExtractOutcome {
    pub files: usize,
    pub bytes: u64,
    pub skipped: usize,
    pub missing: usize,
    pub errors: Vec<String>,
    pub out: PathBuf,
}

fn under_dir(path: &str, dir: &str) -> bool {
    if dir.is_empty() {
        return true;
    }
    path.len() > dir.len()
        && path.starts_with(dir)
        && path.as_bytes().get(dir.len()) == Some(&b'/')
}

fn cmp_entries(a: &Entry, b: &Entry, sort: Sort) -> Ordering {
    let dirs = b.is_dir().cmp(&a.is_dir());
    if dirs != Ordering::Equal {
        return dirs;
    }
    let by_name = || cmp_ci(a.name(), b.name()).then_with(|| a.name().cmp(b.name()));
    match sort {
        Sort::Name => by_name(),
        Sort::Size => b.size.cmp(&a.size).then_with(by_name),
        Sort::Modified => b.modified.cmp(&a.modified).then_with(by_name),
    }
}

fn cmp_ci(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().flat_map(char::to_lowercase);
    let mut b = b.chars().flat_map(char::to_lowercase);
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match x.cmp(&y) {
                Ordering::Equal => {}
                other => return other,
            },
        }
    }
}

fn ci_contains(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.is_ascii() {
        if haystack.len() < needle.len() {
            return false;
        }
        haystack
            .as_bytes()
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    } else {
        haystack.to_lowercase().contains(needle)
    }
}

/// `<backup>/<id[0..2]>/<id>`; an empty or short id falls back to `<backup>/<id>`.
pub fn blob_path(backup_dir: &Path, file_id: &str) -> PathBuf {
    backup_dir
        .join(file_id.get(..2).unwrap_or(""))
        .join(file_id)
}

// ---------------------------------------------------------------------------
// Finding the backup folder
// ---------------------------------------------------------------------------

/// Accept the `<UDID>` folder, a `<DeviceName>_<UDID8>` device folder, a
/// `backup` folder containing several device folders, or a parent such as
/// `~/iPhoneBackups` with one device folder inside.
pub fn resolve_root(input: &Path) -> Result<PathBuf> {
    let mut path = input.to_path_buf();
    if path.is_file() {
        // Dropped Manifest.plist/Manifest.db → use its folder.
        path.pop();
    }
    if is_backup(&path) {
        return Ok(path);
    }
    if path.file_name().map(|n| n == "backup").unwrap_or(false) {
        if let Some(found) = newest_backup(&path) {
            return Ok(found);
        }
    }
    let backup = path.join("backup");
    if backup.is_dir() {
        if let Some(found) = newest_backup(&backup) {
            return Ok(found);
        }
    }
    if let Some(found) = newest_backup(&path) {
        return Ok(found);
    }
    bail!(
        "No iOS backup found in {}.\n\nPick the folder that contains Manifest.plist and \
         Manifest.db (usually <device folder>/backup/<UDID>).",
        input.display()
    )
}

fn is_backup(dir: &Path) -> bool {
    dir.join("Manifest.plist").is_file() && dir.join("Manifest.db").is_file()
}

fn newest_backup(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let read = fs::read_dir(dir).ok()?;
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() || !is_backup(&path) {
            continue;
        }
        let stamp = fs::metadata(path.join("Manifest.plist"))
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| stamp > *t).unwrap_or(true) {
            best = Some((stamp, path));
        }
    }
    best.map(|(_, p)| p)
}

pub fn format_time(unix: i64) -> String {
    DateTime::from_timestamp(unix, 0)
        .map(|t| {
            t.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// Friendly title for a domain, falling back to the raw domain name.
pub fn friendly_domain(domain: &str) -> String {
    let known = match domain {
        "CameraRollDomain" => Some("Camera Roll & Photos"),
        "MediaDomain" => Some("Media library"),
        "MediaDomainNoBackup" => Some("Media (not backed up)"),
        "HomeDomain" => Some("Home & settings"),
        "RootDomain" => Some("System files"),
        "SystemPreferencesDomain" => Some("System preferences"),
        "ManagedPreferencesDomain" => Some("Managed preferences"),
        "KeyboardDomain" => Some("Keyboards"),
        "WirelessDomain" => Some("Wireless settings"),
        "HealthDomain" => Some("Health"),
        "BooksDomain" => Some("Books"),
        "PodcastDomain" => Some("Podcasts"),
        "MobileMailDomain" => Some("Mail"),
        "CalendarDomain" => Some("Calendar"),
        "NotesDomain" => Some("Notes"),
        "SafariDomain" => Some("Safari"),
        "VoicemailDomain" => Some("Voicemail"),
        "CallHistoryDomain" => Some("Call history"),
        "SMSDomain" => Some("Text messages"),
        "MMSDomain" => Some("Multimedia messages"),
        "AppDomain-com.apple.MobileSMS" => Some("Messages"),
        "AppDomain-com.apple.MobileAddressBook" => Some("Contacts"),
        "AppDomain-com.apple.mobilephone" => Some("Phone"),
        "AppDomain-com.apple.mobilecal" => Some("Calendar"),
        "AppDomain-com.apple.mobilenotes" => Some("Notes"),
        "AppDomain-com.apple.MobileMail" => Some("Mail"),
        "AppDomain-com.apple.mobileslideshow" => Some("Photos"),
        "AppDomain-com.apple.Music" => Some("Music"),
        "AppDomain-com.apple.Maps" => Some("Maps"),
        "AppDomain-com.apple.Preferences" => Some("Settings"),
        "AppDomain-com.apple.Health" => Some("Health"),
        "AppDomain-com.apple.calculator" => Some("Calculator"),
        "AppDomain-com.apple.clock" => Some("Clock"),
        "AppDomain-com.apple.weather" => Some("Weather"),
        "AppDomain-com.apple.reminders" => Some("Reminders"),
        "AppDomain-com.apple.mobiletimer" => Some("Timer"),
        "AppDomain-com.apple.DocumentsApp" => Some("Files"),
        "AppDomain-com.apple.shortcuts" => Some("Shortcuts"),
        "AppDomain-net.whatsapp.WhatsApp" => Some("WhatsApp"),
        "AppDomain-org.whispersystems.signal" => Some("Signal"),
        "AppDomain-com.instagram.Instagram" => Some("Instagram"),
        "AppDomain-com.facebook.Facebook" => Some("Facebook"),
        "AppDomain-com.google.Gmail" => Some("Gmail"),
        "AppDomain-com.spotify.client" => Some("Spotify"),
        "AppDomain-com.tinyspeck.chatlyio" => Some("Slack"),
        "AppDomain-com.microsoft.Office.Outlook" => Some("Outlook"),
        "AppDomain-com.google.Maps" => Some("Google Maps"),
        _ => None,
    };
    if let Some(name) = known {
        return name.to_string();
    }
    if let Some(rest) = domain.strip_prefix("AppDomainGroup-") {
        return format!("Shared · {}", bundle_label(rest));
    }
    if let Some(rest) = domain.strip_prefix("AppDomainPlugin-") {
        return format!("Plugin · {}", bundle_label(rest));
    }
    if let Some(rest) = domain.strip_prefix("AppDomain-") {
        return bundle_label(rest);
    }
    if let Some(rest) = domain.strip_prefix("SysContainerDomain-") {
        return format!("System · {}", bundle_label(rest));
    }
    if let Some(rest) = domain.strip_prefix("SysSharedContainerDomain-") {
        return format!("System shared · {}", bundle_label(rest));
    }
    domain.to_string()
}

/// Last meaningful component of a bundle id: `net.whatsapp.WhatsApp` → "WhatsApp".
fn bundle_label(bundle: &str) -> String {
    let last = bundle.rsplit('.').next().unwrap_or(bundle);
    let mut chars = last.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => bundle.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ibackup-explorer-{tag}-{}-{nanos}", std::process::id()))
    }

    fn file_meta(size: u64, modified: u64) -> Vec<u8> {
        let mut dict = plist::Dictionary::new();
        dict.insert("Size".into(), plist::Value::from(size));
        dict.insert("LastModified".into(), plist::Value::from(modified));
        let mut bytes = Vec::new();
        plist::Value::Dictionary(dict)
            .to_writer_binary(&mut bytes)
            .unwrap();
        bytes
    }

    /// Build a tiny but realistic backup: two domains, a nested folder, a
    /// missing ancestor folder row, one blob, and one missing blob.
    fn fixture(tag: &str) -> PathBuf {
        let root = unique_dir(tag);
        fs::create_dir_all(&root).unwrap();
        let mut manifest = plist::Dictionary::new();
        manifest.insert("Device Name".into(), "Test iPhone".into());
        manifest.insert("Product Type".into(), "iPhone15,2".into());
        manifest.insert("Product Version".into(), "17.4".into());
        manifest.insert("Unique Identifier".into(), "000081100000000000000000".into());
        manifest.insert("IsEncrypted".into(), false.into());
        manifest.insert("Version".into(), 10u64.into());
        manifest.insert(
            "Last Backup Date".into(),
            plist::Value::Date(plist::Date::from(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            )),
        );
        plist::Value::Dictionary(manifest)
            .to_file_xml(root.join("Manifest.plist"))
            .unwrap();

        let db = rusqlite::Connection::open(root.join("Manifest.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE Files (fileID TEXT PRIMARY KEY, domain TEXT, relativePath TEXT, flags INTEGER, file BLOB);",
        )
        .unwrap();
        let rows: [(&str, &str, &str, i64, bool); 5] = [
            ("aa11", "CameraRollDomain", "Media/DCIM", 2, true),
            ("bb22", "CameraRollDomain", "Media/DCIM/100APPLE/IMG_0001.HEIC", 1, true),
            ("cc33", "CameraRollDomain", "Media/PhotoData/Thumbnails", 2, false),
            ("dd44", "CameraRollDomain", "Media/PhotoData/Thumbnails/1.jpg", 1, false),
            ("ee55", "AppDomain-com.apple.MobileSMS", "Documents/sms.db", 1, true),
        ];
        for (id, domain, path, flags, present) in rows {
            db.execute(
                "INSERT INTO Files (fileID, domain, relativePath, flags, file) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![id, domain, path, flags, file_meta(2048, 1_700_000_100)],
            )
            .unwrap();
            if present {
                let dir = root.join(&id[..2]);
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join(id), b"blob-bytes").unwrap();
            }
        }
        // A file whose ancestor folder rows are absent entirely.
        db.execute(
            "INSERT INTO Files (fileID, domain, relativePath, flags, file) VALUES ('ff66','AppDomain-com.apple.MobileSMS','Library/SMS/Attachments/deep.dat',1,?1)",
            rusqlite::params![file_meta(10, 1_700_000_200)],
        )
        .unwrap();
        root
    }

    #[test]
    fn opens_lists_and_searches() {
        let root = fixture("open");
        let backup = Backup::open(&root).unwrap();
        assert_eq!(backup.info.device_name, "Test iPhone");
        assert!(!backup.info.encrypted);
        assert_eq!(backup.file_count, 4);
        assert_eq!(backup.dir_count, 9); // two real folders + seven invented ancestors

        let cam = backup.domain_id("CameraRollDomain").unwrap();
        let root_ids = backup.listing(cam, "", KindFilter::All, Sort::Name);
        assert_eq!(root_ids.len(), 1); // Media
        let media = &backup.entries[root_ids[0] as usize];
        assert!(media.is_dir());

        let dcim = backup.listing(cam, "Media/DCIM", KindFilter::All, Sort::Name);
        assert_eq!(dcim.len(), 1);
        assert_eq!(backup.entries[dcim[0] as usize].name(), "100APPLE");

        let photos = backup.listing(cam, "Media/DCIM/100APPLE", KindFilter::All, Sort::Name);
        assert_eq!(photos.len(), 1);
        assert_eq!(backup.entries[photos[0] as usize].kind(), Kind::Image);

        let (hits, truncated) = backup.search("img_0001", None, KindFilter::All, 100);
        assert_eq!(hits.len(), 1);
        assert!(!truncated);
        assert_eq!(backup.entries[hits[0] as usize].name(), "IMG_0001.HEIC");

        let (hits, _) = backup.search("sms.db", None, KindFilter::Other, 100);
        assert_eq!(hits.len(), 1);
        assert_eq!(backup.entries[hits[0] as usize].domain, backup.domain_id("AppDomain-com.apple.MobileSMS").unwrap());

        let (missing, _) = backup.search("1.jpg", None, KindFilter::Images, 100);
        assert_eq!(missing.len(), 1);
        assert!(!backup.blob_exists(&backup.entries[missing[0] as usize]));
    }

    #[test]
    fn synthesizes_missing_folders_and_extracts() {
        let root = fixture("extract");
        let backup = Backup::open(&root).unwrap();
        let sms = backup.domain_id("AppDomain-com.apple.MobileSMS").unwrap();
        // "Library", "Library/SMS" and "Library/SMS/Attachments" did not exist in the DB.
        let lib = backup.listing(sms, "", KindFilter::All, Sort::Name);
        assert_eq!(lib.len(), 2); // Documents, Library
        assert!(backup
            .listing(sms, "Library/SMS/Attachments", KindFilter::All, Sort::Name)
            .len()
            == 1);
        let deep = backup.files_under(sms, "Library");
        assert_eq!(deep.len(), 1);

        let out = root.join("out");
        let cam = backup.domain_id("CameraRollDomain").unwrap();
        let ids = backup.listing(cam, "Media/DCIM/100APPLE", KindFilter::All, Sort::Name);
        let outcome = backup.extract(&ids, &out, &mut |_, _, _| {});
        assert_eq!(outcome.files, 1);
        assert_eq!(outcome.missing, 0);
        assert!(out
            .join("CameraRollDomain/Media/DCIM/100APPLE/IMG_0001.HEIC")
            .is_file());

        // Missing blob is reported, not fatal.
        let thumbs = backup.listing(cam, "Media/PhotoData/Thumbnails", KindFilter::All, Sort::Name);
        let outcome = backup.extract(&thumbs, &out, &mut |_, _, _| {});
        assert_eq!(outcome.files, 0);
        assert_eq!(outcome.missing, 1);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resolves_udid_and_device_folders() {
        let root = fixture("resolve");
        let device = root.parent().unwrap().join(format!(
            "TestPhone_ABCDEF12-{}",
            root.file_name().unwrap().to_string_lossy()
        ));
        fs::create_dir_all(device.join("backup")).unwrap();
        // Move the fixture under backup/<UDID>.
        let udid = device.join("backup").join("000081100000000000000000");
        fs::rename(&root, &udid).unwrap();

        assert_eq!(resolve_root(&udid).unwrap(), udid);
        assert_eq!(resolve_root(&device).unwrap(), udid);
        assert_eq!(resolve_root(device.join("backup").as_path()).unwrap(), udid);
        assert_eq!(resolve_root(&device.join("Manifest.plist").as_path()).ok(), None);
        assert!(resolve_root(&device.join("nope")).is_err());

        let backup = Backup::open(&device).unwrap();
        assert_eq!(backup.dir, udid);
        assert_eq!(backup.info.device_name, "Test iPhone");
        fs::remove_dir_all(&device).ok();
    }

    #[test]
    fn rejects_encrypted() {
        let root = fixture("enc");
        let manifest = root.join("Manifest.plist");
        let mut dict: plist::Dictionary = plist::from_file(&manifest).unwrap();
        dict.insert("IsEncrypted".into(), true.into());
        plist::Value::Dictionary(dict).to_file_xml(&manifest).unwrap();
        let err = match Backup::open(&root) {
            Ok(_) => panic!("encrypted backup was opened"),
            Err(err) => err,
        };
        assert!(err.message.contains("encrypted"));
        assert!(err.info.unwrap().encrypted);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn friendly_names() {
        assert_eq!(friendly_domain("CameraRollDomain"), "Camera Roll & Photos");
        assert_eq!(friendly_domain("AppDomain-com.apple.MobileSMS"), "Messages");
        assert_eq!(friendly_domain("AppDomain-net.whatsapp.WhatsApp"), "WhatsApp");
        assert_eq!(friendly_domain("AppDomain-com.example.myapp"), "Myapp");
        assert_eq!(friendly_domain("WeirdDomain"), "WeirdDomain");
    }
}
