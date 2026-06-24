// wpic — quick & dirty TUI to sort camera SD-card photos into ~/Pictures event folders.
//
// Files are clustered by capture time (file mtime): a gap larger than GAP_HOURS
// between consecutive shots starts a new "occasion". NEF+JPG pairs share an mtime
// so they always land in the same cluster. For each cluster you pick a target
// folder name (an existing same-date folder is suggested), then move or copy.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, Stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};

// ---------- config (~/.config/wpic/config) ----------

const DEFAULT_GAP_HOURS: i64 = 5;
const DEFAULT_RCLONE_DEST: &str = "gdrive:Pictures";

struct Config {
    rclone_dest: String, // rclone remote + base path for uploads
    gap_hours: i64,      // new occasion when shots are > this many hours apart
    pictures: PathBuf,   // library / sort destination
}

static CONFIG: OnceLock<Config> = OnceLock::new();
fn config() -> &'static Config {
    CONFIG.get_or_init(Config::load)
}
fn rclone_dest() -> String {
    config().rclone_dest.clone()
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest)
    } else if s == "~" {
        home_dir()
    } else {
        PathBuf::from(s)
    }
}
fn config_path() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".config"))
        .join("wpic/config")
}

impl Config {
    fn load() -> Config {
        let mut c = Config {
            rclone_dest: DEFAULT_RCLONE_DEST.to_string(),
            gap_hours: DEFAULT_GAP_HOURS,
            pictures: home_dir().join("Pictures"),
        };
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(text) => {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once('=') {
                        let (k, v) = (k.trim(), v.trim());
                        match k {
                            "rclone_dest" if !v.is_empty() => c.rclone_dest = v.to_string(),
                            "gap_hours" => {
                                if let Ok(n) = v.parse::<i64>() {
                                    if n > 0 {
                                        c.gap_hours = n;
                                    }
                                }
                            }
                            "pictures" if !v.is_empty() => c.pictures = expand_tilde(v),
                            _ => {}
                        }
                    }
                }
            }
            Err(_) => {
                // first run: write a commented default template so it's easy to edit
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(
                    &path,
                    format!(
                        "# wpic config\n\
                         \n\
                         # Google Drive upload destination (an rclone remote + base path).\n\
                         # Each occasion uploads to <rclone_dest>/<occasion name>.\n\
                         rclone_dest = {DEFAULT_RCLONE_DEST}\n\
                         \n\
                         # Start a new occasion when consecutive shots are more than this\n\
                         # many hours apart.\n\
                         gap_hours = {DEFAULT_GAP_HOURS}\n\
                         \n\
                         # Where sorted occasions live (card mode) and are managed (library mode).\n\
                         pictures = ~/Pictures\n",
                    ),
                );
            }
        }
        // env override wins, handy for one-off runs
        if let Ok(d) = std::env::var("WPIC_RCLONE_DEST") {
            if !d.is_empty() {
                c.rclone_dest = d;
            }
        }
        c
    }
}
// Full-resolution rotated copies live here (can be large → use disk cache, not tmpfs).
fn view_cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".cache")
        });
    base.join("wpic/views")
}

// Read the EXIF orientation (1-8) from a JPEG or a NEF/TIFF, no subprocess.
fn media_orientation(path: &Path) -> u8 {
    use std::io::Read;
    let mut buf = vec![0u8; 65536];
    let n = match fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return 1,
    };
    let d = &buf[..n];
    // NEF/TIFF: the file starts with a TIFF header; orientation is in IFD0
    if d.len() >= 2 && (&d[0..2] == b"II" || &d[0..2] == b"MM") {
        return tiff_orientation(d).unwrap_or(1);
    }
    if d.len() < 4 || d[0] != 0xFF || d[1] != 0xD8 {
        return 1;
    }
    let mut i = 2;
    while i + 4 <= d.len() && d[i] == 0xFF {
        let marker = d[i + 1];
        if marker == 0xDA {
            break; // start of scan: pixel data follows
        }
        let len = ((d[i + 2] as usize) << 8) | d[i + 3] as usize;
        if marker == 0xE1 {
            let s = i + 4;
            if d.len() >= s + 6 && &d[s..s + 4] == b"Exif" {
                if let Some(o) = tiff_orientation(&d[s + 6..]) {
                    return o;
                }
            }
        }
        i += 2 + len;
    }
    1
}

fn tiff_orientation(t: &[u8]) -> Option<u8> {
    if t.len() < 8 {
        return None;
    }
    let le = match &t[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let rd16 = |o: usize| -> u16 {
        if o + 2 > t.len() {
            return 0;
        }
        if le { u16::from_le_bytes([t[o], t[o + 1]]) } else { u16::from_be_bytes([t[o], t[o + 1]]) }
    };
    let rd32 = |o: usize| -> u32 {
        if o + 4 > t.len() {
            return 0;
        }
        if le {
            u32::from_le_bytes([t[o], t[o + 1], t[o + 2], t[o + 3]])
        } else {
            u32::from_be_bytes([t[o], t[o + 1], t[o + 2], t[o + 3]])
        }
    };
    let ifd = rd32(4) as usize;
    let n = rd16(ifd) as usize;
    for k in 0..n {
        let e = ifd + 2 + k * 12;
        if e + 12 > t.len() {
            break;
        }
        if rd16(e) == 0x0112 {
            let v = rd16(e + 8);
            if (1..=8).contains(&v) {
                return Some(v as u8);
            }
        }
    }
    None
}
fn cull_file() -> PathBuf {
    std::env::temp_dir().join("wpic-cull.txt")
}
fn rates_file() -> PathBuf {
    std::env::temp_dir().join("wpic-rates.txt")
}
fn restore_file() -> PathBuf {
    std::env::temp_dir().join("wpic-restore.txt")
}

// What to upload for a given occasion.
#[derive(Clone, Copy, PartialEq)]
enum UploadMode {
    No,
    All,    // whole folder (photos + movies)
    Photos, // jpg + nef only (skip movies)
    Rated,  // photos rated >= the current min-rating
}

// darktable-compatible XMP sidecar path: "IMG.NEF" -> "IMG.NEF.xmp"
fn sidecar_path(img: &Path) -> PathBuf {
    let mut s = img.as_os_str().to_os_string();
    s.push(".xmp");
    PathBuf::from(s)
}

// Read xmp:Rating (1..5) from a sidecar; handles both attribute (xmp:Rating="3")
// and element (<xmp:Rating>3</xmp:Rating>) forms. Returns None for 0/reject/absent.
fn read_xmp_rating(sidecar: &Path) -> Option<u8> {
    let s = fs::read_to_string(sidecar).ok()?;
    let pos = s.find("xmp:Rating")?;
    let after = &s[pos + "xmp:Rating".len()..];
    let b = after.as_bytes();
    let lim = b.len().min(16);
    let mut i = 0;
    while i < lim && !(b[i].is_ascii_digit() || b[i] == b'-') {
        i += 1;
    }
    if i >= lim {
        return None;
    }
    let start = i;
    let mut j = i;
    if b[j] == b'-' {
        j += 1;
    }
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    let val: i32 = after.get(start..j)?.parse().ok()?;
    if (1..=5).contains(&val) {
        Some(val as u8)
    } else {
        None
    }
}

// Load existing ratings from XMP sidecars (written by wpic or darktable).
fn load_ratings(files: &[FileItem]) -> HashMap<String, u8> {
    let mut m = HashMap::new();
    for f in files {
        if !matches!(f.ext.as_str(), "nef" | "jpg" | "jpeg") {
            continue;
        }
        let side = sidecar_path(&f.path);
        if side.exists() {
            if let Some(r) = read_xmp_rating(&side) {
                m.insert(Cluster::stem_of(&f.path), r);
            }
        }
    }
    m
}

// Set Xmp.xmp.Rating (0-5) for darktable. Updates an existing sidecar in place
// via exiv2 (preserving any darktable edits); creates a minimal one otherwise.
fn write_rating_xmp(img: &Path, rating: u8) {
    let side = sidecar_path(img);
    if side.exists() {
        let _ = Command::new("exiv2")
            .arg("-M")
            .arg(format!("set Xmp.xmp.Rating {}", rating))
            .arg(&side)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else if rating > 0 {
        let xmp = format!(
            "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
             <rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\">\n   \
             <xmp:Rating>{rating}</xmp:Rating>\n  </rdf:Description>\n \
             </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>\n"
        );
        let _ = fs::write(&side, xmp);
    }
}

fn is_jpg(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg")
}

// One viewable source per shot stem: prefer the JPG, fall back to the NEF (whose
// embedded preview we extract). Sorted by stem. Movies are ignored.
fn pick_viewables<'a>(files: impl Iterator<Item = &'a FileItem>) -> Vec<PathBuf> {
    let mut by_stem: BTreeMap<String, (Option<PathBuf>, Option<PathBuf>)> = BTreeMap::new();
    for f in files {
        let stem = f.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let slot = by_stem.entry(stem).or_default();
        match f.ext.as_str() {
            "jpg" | "jpeg" => slot.0 = Some(f.path.clone()),
            "nef" => slot.1 = Some(f.path.clone()),
            _ => {}
        }
    }
    by_stem.into_values().filter_map(|(jpg, nef)| jpg.or(nef)).collect()
}

// ---------- data model ----------

struct FileItem {
    path: PathBuf,
    mtime: i64,  // unix seconds
    ext: String, // lowercase, no dot
    size: u64,   // bytes
}

fn human_size(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{:.1} {}", v, U[i])
    }
}

struct Cluster {
    files: Vec<FileItem>,
    start: i64,
    end: i64,
    target: String,  // folder name under ~/Pictures (empty desc => needs attention)
    suggested: bool, // target came from an existing folder
    skip: bool,
    up: UploadMode,            // what to upload to Drive
    ratings: HashMap<String, u8>, // basename stem -> rating 1..5 (set in imv)
    dir: PathBuf,              // folder the files live in (holds the .removed subfolder)
    removed: usize,            // count of removed shots in dir/.removed
    on_drive: Option<bool>,    // None = unknown, Some(true/false) after a Drive check
}

impl Cluster {
    fn count_by_ext(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for f in &self.files {
            *m.entry(f.ext.clone()).or_insert(0) += 1;
        }
        m
    }
    fn stem_of(p: &Path) -> String {
        p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    }
    fn rating_of(&self, p: &Path) -> u8 {
        self.ratings.get(&Self::stem_of(p)).copied().unwrap_or(0)
    }
    // total bytes of the occasion's media (excludes .removed, which isn't in `files`)
    fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
    // distinct photo shots (a jpg+nef pair counts once)
    fn pic_count(&self) -> usize {
        let mut stems = HashSet::new();
        for f in &self.files {
            if Self::is_photo(f) {
                stems.insert(Self::stem_of(&f.path));
            }
        }
        stems.len()
    }
    fn movie_count(&self) -> usize {
        self.files.iter().filter(|f| matches!(f.ext.as_str(), "mov" | "mp4")).count()
    }
    // sorted movie paths (mpv playlist)
    fn movies(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|f| matches!(f.ext.as_str(), "mov" | "mp4"))
            .map(|f| f.path.clone())
            .collect();
        v.sort();
        v
    }
    // one viewable source per shot (jpg if present, else the NEF); when `min > 0`
    // restrict to shots rated >= min
    fn viewable_sources(&self, min: u8) -> Vec<PathBuf> {
        pick_viewables(self.files.iter().filter(|f| min == 0 || self.rating_of(&f.path) >= min))
    }
    // number of distinct shots rated >= min
    fn rated_count(&self, min: u8) -> usize {
        self.ratings.values().filter(|&&r| r >= min).count()
    }
    fn is_photo(f: &FileItem) -> bool {
        matches!(f.ext.as_str(), "jpg" | "jpeg" | "nef")
    }
    // basenames of all photos (jpg+nef), skipping movies
    fn photo_filenames(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|f| Self::is_photo(f))
            .filter_map(|f| f.path.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect()
    }
    // basenames of photos whose shot is rated >= min — for upload filtering
    fn rated_filenames(&self, min: u8) -> Vec<String> {
        self.files
            .iter()
            .filter(|f| Self::is_photo(f) && self.rating_of(&f.path) >= min)
            .filter_map(|f| f.path.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect()
    }
    // forget ratings whose stem no longer has a file present
    fn prune_ratings(&mut self) {
        let present: HashSet<String> = self.files.iter().map(|f| Self::stem_of(&f.path)).collect();
        self.ratings.retain(|s, _| present.contains(s));
    }
    // drop files whose basename stem is in `stems`; refresh start/end; auto-skip if empty
    fn remove_stems(&mut self, stems: &std::collections::HashSet<String>) {
        self.files.retain(|f| {
            let stem = f
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            !stems.contains(&stem)
        });
        self.start = self.files.first().map(|f| f.mtime).unwrap_or(self.start);
        self.end = self.files.last().map(|f| f.mtime).unwrap_or(self.end);
        if self.files.is_empty() {
            self.skip = true;
        }
    }
    fn ext_summary(&self) -> String {
        self.count_by_ext()
            .iter()
            .map(|(k, v)| format!("{} {}", v, k))
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn date_str(&self) -> String {
        fmt_date(self.start)
    }
}

// ---------- date formatting using captured local offset ----------

static mut LOCAL_OFFSET: i64 = 0;
fn local_offset() -> i64 {
    unsafe { LOCAL_OFFSET }
}

fn fmt_date_time(secs: i64, offset: i64) -> (String, String) {
    let t = secs + offset;
    let days = t.div_euclid(86400);
    let rem = t.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    (
        format!("{:04}-{:02}-{:02}", y, m, d),
        format!("{:02}:{:02}", rem / 3600, (rem % 3600) / 60),
    )
}
fn fmt_date(secs: i64) -> String {
    fmt_date_time(secs, local_offset()).0
}
fn fmt_time(secs: i64) -> String {
    fmt_date_time(secs, local_offset()).1
}

// Howard Hinnant's civil_from_days
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------- scanning & clustering ----------

fn is_media(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "nef" | "mov" | "mp4" | "raw")
}

fn read_item(entry: &fs::DirEntry) -> Option<FileItem> {
    let p = entry.path();
    if !entry.file_type().ok()?.is_file() {
        return None;
    }
    let ext = p.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).unwrap_or_default();
    if !is_media(&ext) {
        return None;
    }
    let md = entry.metadata().ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    Some(FileItem { path: p, mtime, ext, size: md.len() })
}

fn file_item(path: PathBuf) -> Option<FileItem> {
    let ext = path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).unwrap_or_default();
    let md = fs::metadata(&path).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    Some(FileItem { path, mtime, ext, size: md.len() })
}

// number of removed shots (jpgs) in <dir>/.removed
fn count_removed(dir: &Path) -> usize {
    scan_dir_images(&dir.join(".removed")).iter().filter(|f| is_jpg(&f.ext)).count()
}

// Flat scan of media files directly inside one directory.
fn scan_dir_images(dir: &Path) -> Vec<FileItem> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Some(it) = read_item(&e) {
                out.push(it);
            }
        }
    }
    out.sort_by_key(|f| f.mtime);
    out
}

fn scan(dcim: &Path) -> io::Result<Vec<FileItem>> {
    let mut out = Vec::new();
    for roll in fs::read_dir(dcim)? {
        let roll = roll?;
        if !roll.file_type()?.is_dir() {
            continue;
        }
        for f in fs::read_dir(roll.path())? {
            if let Some(it) = read_item(&f?) {
                out.push(it);
            }
        }
    }
    out.sort_by_key(|f| f.mtime);
    Ok(out)
}

// Library mode: every immediate subfolder of ~/Pictures that holds media becomes
// an already-named "occasion" (newest first). No moving — only review/cull/upload.
fn scan_library(pictures: &Path) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    if let Ok(rd) = fs::read_dir(pictures) {
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let files = scan_dir_images(&dir);
            if files.is_empty() {
                continue;
            }
            let start = files.first().map(|f| f.mtime).unwrap_or(0);
            let end = files.last().map(|f| f.mtime).unwrap_or(0);
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let removed = count_removed(&dir);
            let ratings = load_ratings(&files);
            clusters.push(Cluster {
                files,
                start,
                end,
                target: name,
                suggested: true,
                skip: false,
                up: UploadMode::No,
                ratings,
                dir: dir.clone(),
                removed,
                on_drive: None,
            });
        }
    }
    clusters.sort_by(|a, b| b.target.cmp(&a.target)); // newest dated folder first
    clusters
}

fn cluster(files: Vec<FileItem>, pictures: &Path) -> Vec<Cluster> {
    let gap = config().gap_hours * 3600;
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut cur: Vec<FileItem> = Vec::new();
    let mut last = i64::MIN;
    for f in files {
        if !cur.is_empty() && f.mtime - last > gap {
            clusters.push(finish(std::mem::take(&mut cur), pictures));
        }
        last = f.mtime;
        cur.push(f);
    }
    if !cur.is_empty() {
        clusters.push(finish(cur, pictures));
    }
    clusters
}

fn finish(files: Vec<FileItem>, pictures: &Path) -> Cluster {
    let start = files.first().map(|f| f.mtime).unwrap_or(0);
    let end = files.last().map(|f| f.mtime).unwrap_or(0);
    let date = fmt_date(start);
    let mut target = format!("{} ", date);
    let mut suggested = false;
    if let Ok(rd) = fs::read_dir(pictures) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    if name.starts_with(&date) && name.len() > date.len() {
                        target = name.to_string();
                        suggested = true;
                        break;
                    }
                }
            }
        }
    }
    let dir = files
        .first()
        .and_then(|f| f.path.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| pictures.to_path_buf());
    let removed = count_removed(&dir);
    let ratings = load_ratings(&files);
    Cluster {
        files, start, end, target, suggested, skip: false,
        up: UploadMode::No, ratings, dir, removed, on_drive: None,
    }
}

// the part of the target after the "YYYY-MM-DD " prefix
fn desc_part(target: &str) -> &str {
    if target.len() >= 11 && target.as_bytes().get(10) == Some(&b' ') {
        &target[11..]
    } else {
        target
    }
}

// ---------- app state ----------

#[derive(PartialEq)]
enum Mode {
    Browse,
    Edit,
    Confirm,
    Running,
    Done,
}

// shared progress between the worker thread and the UI
struct Progress {
    stage: Mutex<String>, // e.g. "Moving files" / "Uploading to Drive"
    total: AtomicUsize,   // units in the current stage
    done: AtomicUsize,
    frac: AtomicU64,      // fraction of the current unit, in basis points (0..=10000)
    errors: AtomicUsize,
    finished: AtomicBool,
    current: Mutex<String>,
    log: Mutex<Vec<String>>,
}

impl Progress {
    fn set_stage(&self, name: &str, total: usize) {
        if let Ok(mut s) = self.stage.lock() {
            *s = name.to_string();
        }
        self.total.store(total, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        self.frac.store(0, Ordering::Relaxed);
    }
}

struct App {
    pictures: PathBuf,
    clusters: Vec<Cluster>,
    list: ListState,
    mode: Mode,
    edit_buf: String,
    do_move: bool,  // true = move, false = copy (card mode only)
    library: bool,  // true = managing existing ~/Pictures folders (no card)
    min_rating: u8, // threshold (1-5) for "rated" review/upload
    log: Vec<String>,
    progress: Option<Arc<Progress>>,
    notice: Option<String>, // transient full-screen message (e.g. launching viewer)
    status: String,         // last-action status shown in the detail title
    // background Drive check started at launch: None while running, then Some(result)
    drive_check: Arc<Mutex<Option<Result<HashSet<String>, String>>>>,
    drive_applied: bool,
}

impl App {
    fn sel(&self) -> usize {
        self.list.selected().unwrap_or(0)
    }
    fn assigned_count(&self) -> usize {
        self.clusters
            .iter()
            .filter(|c| !c.skip && !desc_part(&c.target).trim().is_empty())
            .count()
    }
    fn upload_count(&self) -> usize {
        self.clusters
            .iter()
            .filter(|c| c.up != UploadMode::No && !c.skip && !desc_part(&c.target).trim().is_empty())
            .count()
    }
}

// ---------- file ops ----------

fn move_or_copy(src: &Path, dst: &Path, do_move: bool) -> io::Result<()> {
    if do_move {
        if fs::rename(src, dst).is_ok() {
            return Ok(());
        }
        fs::copy(src, dst)?;
        fs::remove_file(src)?;
        return Ok(());
    }
    fs::copy(src, dst)?;
    Ok(())
}

// One folder's worth of work, prepared on the UI thread before spawning.
struct Job {
    dir: PathBuf,
    label: String,
    srcs: Vec<PathBuf>,
    upload: bool,
    upload_files: Option<Vec<String>>, // None = whole folder; Some = only these basenames
}

// Kick off the move/copy (and any uploads) on a background thread.
fn start_execute(app: &mut App) {
    let mut jobs: Vec<Job> = Vec::new();
    let mut total = 0usize; // files to move (0 in library mode)
    for c in &app.clusters {
        if c.skip || desc_part(&c.target).trim().is_empty() {
            continue;
        }
        let upload_files = match c.up {
            UploadMode::No => None,
            UploadMode::All => Some(None),
            UploadMode::Photos => Some(Some(c.photo_filenames())),
            UploadMode::Rated => Some(Some(c.rated_filenames(app.min_rating))),
        };
        if app.library {
            // nothing to move; only enqueue folders marked for upload
            let Some(filter) = upload_files else { continue };
            jobs.push(Job {
                dir: app.pictures.join(c.target.trim_end()),
                label: c.target.trim_end().to_string(),
                srcs: Vec::new(),
                upload: true,
                upload_files: filter,
            });
        } else {
            let srcs: Vec<PathBuf> = c.files.iter().map(|f| f.path.clone()).collect();
            total += srcs.len();
            let (upload, filter) = match upload_files {
                Some(f) => (true, f),
                None => (false, None),
            };
            jobs.push(Job {
                dir: app.pictures.join(c.target.trim_end()),
                label: c.target.trim_end().to_string(),
                srcs,
                upload,
                upload_files: filter,
            });
        }
    }

    let init_stage = if total == 0 { "Uploading to Drive" } else { "Moving files" };
    let init_total = if total == 0 { jobs.len() } else { total };
    let progress = Arc::new(Progress {
        stage: Mutex::new(init_stage.to_string()),
        total: AtomicUsize::new(init_total),
        done: AtomicUsize::new(0),
        frac: AtomicU64::new(0),
        errors: AtomicUsize::new(0),
        finished: AtomicBool::new(false),
        current: Mutex::new(String::new()),
        log: Mutex::new(Vec::new()),
    });
    app.progress = Some(progress.clone());
    app.mode = Mode::Running;

    let do_move = app.do_move;
    let dest = rclone_dest();
    std::thread::spawn(move || worker(jobs, do_move, dest, progress));
}

// One `rclone lsf` lists which occasion folders exist under the dest base.
// A missing base folder => empty set (nothing uploaded yet, not an error).
fn rclone_list_dirs(dest: &str) -> Result<HashSet<String>, String> {
    let out = Command::new("rclone")
        .args(["lsf", "--dirs-only"])
        .arg(dest)
        .output()
        .map_err(|e| format!("rclone not runnable: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("directory not found") {
            return Ok(HashSet::new());
        }
        return Err(err.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim_end_matches('/').to_string())
        .collect())
}

// Mark each cluster on_drive = Some(present). Presence != verified-complete, but
// re-uploading safely tops up any missing files.
fn apply_drive_listing(app: &mut App, listed: &HashSet<String>) -> usize {
    let mut n = 0;
    for c in &mut app.clusters {
        let present = listed.contains(c.target.trim_end());
        c.on_drive = Some(present);
        if present {
            n += 1;
        }
    }
    n
}

fn check_drive(app: &mut App) -> String {
    let dest = rclone_dest();
    match rclone_list_dirs(&dest) {
        Ok(listed) => {
            let n = apply_drive_listing(app, &listed);
            format!("Drive: {n} of {} occasions present under {dest}", app.clusters.len())
        }
        Err(e) => format!("Drive check failed: {e}"),
    }
}

fn worker(jobs: Vec<Job>, do_move: bool, dest: String, p: Arc<Progress>) {
    // ---- stage 1: move/copy into ~/Pictures (skipped entirely in library mode) ----
    let any_move = jobs.iter().any(|j| !j.srcs.is_empty());
    let mut to_upload: Vec<(PathBuf, String, Option<Vec<String>>)> = Vec::new();
    for job in &jobs {
        if !job.srcs.is_empty() {
            if let Err(e) = fs::create_dir_all(&job.dir) {
                p.errors.fetch_add(job.srcs.len(), Ordering::Relaxed);
                push_log(&p, format!("ERR mkdir {}: {}", job.dir.display(), e));
                continue;
            }
        }
        for src in &job.srcs {
            let name = src.file_name().unwrap();
            if let Ok(mut cur) = p.current.lock() {
                *cur = format!("{} → {}", name.to_string_lossy(), job.label);
            }
            let mut dst = job.dir.join(name);
            if dst.exists() {
                let stem = Path::new(name).file_stem().unwrap().to_string_lossy().to_string();
                let ext = src.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
                let mut n = 1;
                loop {
                    let cand = job.dir.join(format!("{}_{}.{}", stem, n, ext));
                    if !cand.exists() {
                        dst = cand;
                        break;
                    }
                    n += 1;
                }
            }
            match move_or_copy(src, &dst, do_move) {
                Ok(()) => {
                    p.done.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    p.errors.fetch_add(1, Ordering::Relaxed);
                    if p.log.lock().map(|l| l.len() < 8).unwrap_or(false) {
                        push_log(&p, format!("ERR {}: {}", src.display(), e));
                    }
                }
            }
        }
        if !job.srcs.is_empty() {
            push_log(&p, format!("{}  ->  {} files", job.label, job.srcs.len()));
        }
        if job.upload {
            to_upload.push((job.dir.clone(), job.label.clone(), job.upload_files.clone()));
        }
    }
    if any_move {
        let verb = if do_move { "Moved" } else { "Copied" };
        push_log(
            &p,
            format!(
                "{} {} files, {} errors.",
                verb,
                p.done.load(Ordering::Relaxed),
                p.errors.load(Ordering::Relaxed)
            ),
        );
    }

    // ---- stage 2: upload marked folders to Drive via rclone ----
    if !to_upload.is_empty() {
        p.set_stage("Uploading to Drive", to_upload.len());
        for (dir, label, filter) in &to_upload {
            let what = match filter {
                Some(f) => format!("{} ({} rated files)", label, f.len()),
                None => label.clone(),
            };
            if let Ok(mut cur) = p.current.lock() {
                *cur = format!("rclone → {}/{}", dest, what);
            }
            match rclone_upload(dir, &format!("{}/{}", dest, label), filter.as_deref(), &p) {
                Ok(()) => {
                    p.done.fetch_add(1, Ordering::Relaxed);
                    push_log(&p, format!("uploaded  {}", what));
                }
                Err(e) => {
                    p.errors.fetch_add(1, Ordering::Relaxed);
                    push_log(&p, format!("UPLOAD ERR {}: {}", label, e));
                }
            }
            p.frac.store(0, Ordering::Relaxed);
        }
        push_log(&p, format!("Uploaded {} folder(s) to {}.", to_upload.len(), dest));
    }

    p.finished.store(true, Ordering::Relaxed);
}

// Run `rclone copy <dir> <dest>` streaming its one-line stats into progress.current.
// When `filter` is Some, only those basenames are uploaded (via --files-from).
fn rclone_upload(dir: &Path, dest: &str, filter: Option<&[String]>, p: &Arc<Progress>) -> io::Result<()> {
    let mut cmd = Command::new("rclone");
    cmd.arg("copy").arg(dir).arg(dest).args([
        "--exclude",
        ".removed/**", // never back up culled shots
        "--transfers=4",
        "--checkers=8",
        "--stats=1s",
        "--stats-one-line",
        "--stats-log-level",
        "NOTICE",
    ]);
    // a per-upload list file so concurrent folders don't clash
    let list_path = std::env::temp_dir().join(format!("wpic-upload-{}.txt", std::process::id()));
    if let Some(names) = filter {
        fs::write(&list_path, names.join("\n"))?;
        cmd.arg("--files-from").arg(&list_path);
    }
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("rclone not runnable ({e})")))?;

    p.frac.store(0, Ordering::Relaxed);
    if let Some(err) = child.stderr.take() {
        for line in io::BufReader::new(err).lines().map_while(Result::ok) {
            if let Some(pct) = parse_percent(&line) {
                p.frac.store((pct * 100.0) as u64, Ordering::Relaxed);
            }
            if let Ok(mut cur) = p.current.lock() {
                *cur = line.trim().to_string();
            }
        }
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("rclone exited with {status}")))
    }
}

fn push_log(p: &Arc<Progress>, msg: String) {
    if let Ok(mut l) = p.log.lock() {
        l.push(msg);
    }
}

// Pull the "NN%" percentage out of an rclone --stats-one-line line.
fn parse_percent(line: &str) -> Option<f64> {
    let pos = line.find('%')?;
    let start = line[..pos]
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    line[start..pos].parse::<f64>().ok().filter(|v| (0.0..=100.0).contains(v))
}

// ---------- review / cull (imv on EXIF-corrected jpg previews) ----------

// Move a culled file into a sibling `.removed/` folder (recoverable, no system
// trash). Brings any darktable .xmp sidecar along. Collisions get a _N suffix.
fn move_to_removed(path: &Path) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let rem = parent.join(".removed");
    fs::create_dir_all(&rem)?;
    let name = path.file_name().unwrap();
    let mut dst = rem.join(name);
    if dst.exists() {
        let stem = Path::new(name).file_stem().unwrap().to_string_lossy().to_string();
        let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        let mut n = 1;
        loop {
            let cand = rem.join(format!("{}_{}.{}", stem, n, ext));
            if !cand.exists() {
                dst = cand;
                break;
            }
            n += 1;
        }
    }
    // relocate a darktable sidecar too, if present
    let side = sidecar_path(path);
    if side.exists() {
        if let Some(sn) = side.file_name() {
            let _ = fs::rename(&side, rem.join(sn));
        }
    }
    if fs::rename(path, &dst).is_ok() {
        return Ok(());
    }
    fs::copy(path, &dst)?;
    fs::remove_file(path)?;
    Ok(())
}

// Build (display, original) pairs for imv: the untouched original for upright
// shots (full quality), and a full-resolution rotated copy (cached by stem) for
// shots that carry only an EXIF orientation tag — imv doesn't auto-orient.
fn generate_views(sources: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let dir = view_cache_dir();
    let _ = fs::create_dir_all(&dir);

    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut todo: Vec<(PathBuf, PathBuf, bool)> = Vec::new(); // (dst, src, is_nef)
    for src in sources {
        let is_nef = src.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("nef")).unwrap_or(false);
        if !is_nef && media_orientation(src) <= 1 {
            out.push((src.clone(), src.clone())); // upright jpg original, shown as-is
            continue;
        }
        let stem = src.file_stem().unwrap().to_string_lossy().to_string();
        let dst = dir.join(format!("{stem}.jpg"));
        if !dst.exists() {
            todo.push((dst.clone(), src.clone(), is_nef));
        }
        out.push((dst, src.clone()));
    }

    if !todo.is_empty() {
        let nthreads = 4usize;
        let chunk = ((todo.len() + nthreads - 1) / nthreads).max(1);
        let mut handles = Vec::new();
        for slice in todo.chunks(chunk) {
            let slice: Vec<(PathBuf, PathBuf, bool)> = slice.to_vec();
            handles.push(std::thread::spawn(move || {
                for (dst, src, is_nef) in &slice {
                    let o = media_orientation(src);
                    if *is_nef {
                        extract_nef_view(src, dst, o);
                    } else {
                        correct_orientation(src, dst, o);
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
    }

    out.into_iter().filter(|(v, _)| v.exists()).collect()
}

// Extract the largest embedded JPEG preview from a NEF (the camera's own full-res
// JPEG) and orient it. The embedded preview carries no orientation tag, so we
// rotate it explicitly by the NEF's orientation.
fn extract_nef_view(nef: &Path, dst: &Path, orientation: u8) {
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let stem = nef.file_stem().unwrap().to_string_lossy().to_string();
    let prefix = format!("{stem}-preview");

    // extract all previews into the cache dir as <stem>-previewN.jpg
    let _ = Command::new("exiv2")
        .args(["-ep", "-l"])
        .arg(dir)
        .arg(nef)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // pick the largest extracted preview, delete the rest
    let mut best: Option<(u64, PathBuf)> = None;
    let mut extras: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if name.starts_with(&prefix) && name.ends_with(".jpg") {
                let sz = e.metadata().map(|m| m.len()).unwrap_or(0);
                if best.as_ref().map_or(true, |(b, _)| sz > *b) {
                    if let Some((_, old)) = best.replace((sz, p.clone())) {
                        extras.push(old);
                    }
                } else {
                    extras.push(p);
                }
            }
        }
    }
    for x in extras {
        let _ = fs::remove_file(x);
    }
    let Some((_, preview)) = best else { return };

    // orient: lossless jpegtran rotate for 90/180/270, else just use the preview
    let rotate = match orientation {
        3 => Some("180"),
        6 => Some("90"),
        8 => Some("270"),
        _ => None,
    };
    if let Some(deg) = rotate {
        let ok = Command::new("jpegtran")
            .args(["-copy", "none", "-trim", "-rotate", deg, "-outfile"])
            .arg(dst)
            .arg(&preview)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && dst.exists() {
            let _ = fs::remove_file(&preview);
            return;
        }
    }
    let _ = fs::rename(&preview, dst);
}

// Write an upright, full-resolution copy of `src` to `dst`. Lossless jpegtran for
// the 90/180/270 cases; vips auto-rotate fallback for rare flips/transposes.
fn correct_orientation(src: &Path, dst: &Path, orientation: u8) {
    let rotate = match orientation {
        3 => Some("180"),
        6 => Some("90"),
        8 => Some("270"),
        _ => None,
    };
    if let Some(deg) = rotate {
        let ok = Command::new("jpegtran")
            .args(["-copy", "none", "-trim", "-rotate", deg, "-outfile"])
            .arg(dst)
            .arg(src)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && dst.exists() {
            return;
        }
    }
    // fallback: vips auto-rotate at original resolution (cap far above sensor size)
    let outpat = format!("{}[Q=92]", view_cache_dir().join("%s.jpg").to_string_lossy());
    let _ = Command::new("vipsthumbnail")
        .arg(src)
        .args(["--size", "12000x12000", "-o"])
        .arg(&outpat)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn read_stems(path: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(s) = Path::new(line).file_stem() {
                set.insert(s.to_string_lossy().to_string());
            }
        }
    }
    set
}

// Parse the rates file ("<rating>\t<path>" lines); last entry per stem wins.
fn read_rates(path: &Path) -> HashMap<String, u8> {
    let mut map = HashMap::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let mut it = line.splitn(2, '\t');
            let (Some(r), Some(p)) = (it.next(), it.next()) else { continue };
            let Ok(rating) = r.trim().parse::<u8>() else { continue };
            if let Some(stem) = Path::new(p.trim()).file_stem() {
                map.insert(stem.to_string_lossy().to_string(), rating.min(5));
            }
        }
    }
    map
}

// Open imv on this occasion's jpg previews. In imv:
//   1..5 = rate (0 clears) and advance,
//   Delete = cull (trashed on exit; view moves forward, not back),
//   u = undo last cull (un-records + reopens).
// Culled shots are moved to a sibling `.removed/` folder (recoverable, not the
// system trash). Ratings are written to darktable XMP sidecars after imv exits.
// `only_rated` (with `min`) restricts the view to shots already rated >= min.
fn review_cluster(c: &mut Cluster, only_rated: bool, min: u8) -> String {
    let sources = c.viewable_sources(if only_rated { min } else { 0 });
    if sources.is_empty() {
        return if only_rated {
            format!("No shots rated ≥{min} in this occasion yet.")
        } else {
            "No photos to view here (movies only).".into()
        };
    }
    let views = generate_views(&sources);
    if views.is_empty() {
        return "Could not prepare images for viewing.".into();
    }

    let cull = cull_file();
    let rates = rates_file();
    let _ = fs::write(&cull, b"");
    let _ = fs::write(&rates, b"");

    // one helper (uses $imv_pid/$imv_current_file/$imv_current_index/$imv_file_count)
    let helper = std::env::temp_dir().join("wpic-imv.sh");
    let _ = fs::write(
        &helper,
        format!(
            "#!/bin/sh\n\
             CULL='{cull}'\n\
             RATES='{rates}'\n\
             case \"$1\" in\n\
             rate) printf '%s\\t%s\\n' \"$2\" \"$3\" >> \"$RATES\"; imv-msg \"$imv_pid\" next 2>/dev/null ;;\n\
             cull)\n\
             \tprintf '%s\\n' \"$2\" >> \"$CULL\"\n\
             \tidx=\"$3\"; cnt=\"$4\"\n\
             \timv-msg \"$imv_pid\" close 2>/dev/null\n\
             \tnew=$((cnt-1)); [ \"$idx\" -gt \"$new\" ] && idx=\"$new\"\n\
             \t[ \"$idx\" -ge 1 ] && imv-msg \"$imv_pid\" goto \"$idx\" 2>/dev/null ;;\n\
             undo) last=$(tail -n1 \"$CULL\" 2>/dev/null); [ -n \"$last\" ] && sed -i '$ d' \"$CULL\" && imv-msg \"$imv_pid\" open \"$last\" 2>/dev/null ;;\n\
             esac\n",
            cull = cull.to_string_lossy(),
            rates = rates.to_string_lossy(),
        ),
    );

    // imv treats each -c as ONE command, so pass one bind per -c flag
    let h = helper.to_string_lossy();
    let mut bind_cmds: Vec<String> = Vec::new();
    for n in 0..=5 {
        bind_cmds.push(format!("bind {n} exec sh '{h}' rate {n} \"$imv_current_file\""));
    }
    bind_cmds.push(format!(
        "bind <Delete> exec sh '{h}' cull \"$imv_current_file\" \"$imv_current_index\" \"$imv_file_count\""
    ));
    bind_cmds.push(format!("bind u exec sh '{h}' undo"));

    let mut cmd = Command::new("imv");
    for b in &bind_cmds {
        cmd.arg("-c").arg(b);
    }
    let view_paths: Vec<PathBuf> = views.iter().map(|(p, _)| p.clone()).collect();
    cmd.args(&view_paths);
    if cmd.status().is_err() {
        return "Failed to launch imv.".into();
    }

    let cull_stems = read_stems(&cull);
    let mut rate_map = read_rates(&rates);

    // apply culls first: move files sharing a culled stem to .removed; don't rate those
    let mut trashed = 0;
    if !cull_stems.is_empty() {
        for f in &c.files {
            if cull_stems.contains(&Cluster::stem_of(&f.path)) && move_to_removed(&f.path).is_ok() {
                trashed += 1;
            }
        }
        for st in &cull_stems {
            let _ = fs::remove_file(view_cache_dir().join(format!("{st}.jpg")));
            rate_map.remove(st);
        }
        c.remove_stems(&cull_stems);
    }

    // apply ratings: update in-memory + write darktable XMP sidecars (nef + jpg)
    let rated = rate_map.len();
    for (stem, rating) in &rate_map {
        if *rating > 0 {
            c.ratings.insert(stem.clone(), *rating);
        } else {
            c.ratings.remove(stem);
        }
        for f in &c.files {
            if Cluster::stem_of(&f.path) == *stem && matches!(f.ext.as_str(), "nef" | "jpg" | "jpeg") {
                write_rating_xmp(&f.path, *rating);
            }
        }
    }
    c.prune_ratings();
    c.removed = count_removed(&c.dir);

    let mut parts: Vec<String> = Vec::new();
    if !cull_stems.is_empty() {
        parts.push(format!("culled {} ({} files → .removed)", cull_stems.len(), trashed));
    }
    if rated > 0 {
        parts.push(format!("rated {rated} (XMP written)"));
    }
    if parts.is_empty() {
        return "Review done — no changes.".into();
    }
    format!("{} · ★≥{} now {}", parts.join(", "), min, c.rated_count(min))
}

// Move a file (and any .xmp sidecar) from .removed back out into `occ`.
fn restore_one(removed_path: &Path, occ: &Path) -> io::Result<PathBuf> {
    let name = removed_path.file_name().unwrap();
    let mut dst = occ.join(name);
    if dst.exists() {
        let stem = Path::new(name).file_stem().unwrap().to_string_lossy().to_string();
        let ext = removed_path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        let mut n = 1;
        loop {
            let cand = occ.join(format!("{}_{}.{}", stem, n, ext));
            if !cand.exists() {
                dst = cand;
                break;
            }
            n += 1;
        }
    }
    let side = sidecar_path(removed_path);
    if side.exists() {
        if let Some(sn) = side.file_name() {
            let _ = fs::rename(&side, occ.join(sn));
        }
    }
    if fs::rename(removed_path, &dst).is_ok() {
        return Ok(dst);
    }
    fs::copy(removed_path, &dst)?;
    fs::remove_file(removed_path)?;
    Ok(dst)
}

// Separate viewer over an occasion's .removed shots; Enter restores the current one.
fn view_removed(c: &mut Cluster) -> String {
    let rem = c.dir.join(".removed");
    let removed = scan_dir_images(&rem);
    if removed.is_empty() {
        return "Nothing in .removed for this occasion.".into();
    }
    let sources = pick_viewables(removed.iter());
    if sources.is_empty() {
        return format!("{} removed file(s) but no photos to preview.", removed.len());
    }
    let views = generate_views(&sources);
    if views.is_empty() {
        return "Could not prepare removed images.".into();
    }

    let restore = restore_file();
    let _ = fs::write(&restore, b"");
    let helper = std::env::temp_dir().join("wpic-restore.sh");
    let _ = fs::write(
        &helper,
        format!(
            "#!/bin/sh\n\
             RESTORE='{r}'\n\
             printf '%s\\n' \"$2\" >> \"$RESTORE\"\n\
             idx=\"$3\"; cnt=\"$4\"\n\
             imv-msg \"$imv_pid\" close 2>/dev/null\n\
             new=$((cnt-1)); [ \"$idx\" -gt \"$new\" ] && idx=\"$new\"\n\
             [ \"$idx\" -ge 1 ] && imv-msg \"$imv_pid\" goto \"$idx\" 2>/dev/null\n",
            r = restore.to_string_lossy()
        ),
    );
    let bind = format!(
        "bind <Return> exec sh '{h}' restore \"$imv_current_file\" \"$imv_current_index\" \"$imv_file_count\"",
        h = helper.to_string_lossy()
    );
    let view_paths: Vec<PathBuf> = views.iter().map(|(p, _)| p.clone()).collect();
    let mut cmd = Command::new("imv");
    cmd.arg("-c").arg(&bind).args(&view_paths);
    if cmd.status().is_err() {
        return "Failed to launch imv.".into();
    }

    let restore_stems = read_stems(&restore);
    let mut restored = 0;
    for f in &removed {
        if !restore_stems.contains(&Cluster::stem_of(&f.path)) {
            continue;
        }
        if let Ok(newp) = restore_one(&f.path, &c.dir) {
            if let Some(item) = file_item(newp) {
                c.files.push(item);
            }
            restored += 1;
        }
    }
    if restored > 0 {
        c.files.sort_by_key(|f| f.mtime);
        c.start = c.files.first().map(|f| f.mtime).unwrap_or(c.start);
        c.end = c.files.last().map(|f| f.mtime).unwrap_or(c.end);
        c.skip = false;
    }
    c.removed = count_removed(&c.dir);
    if restored > 0 {
        format!("Restored {restored} shot(s) from .removed.")
    } else {
        format!("{} shot(s) in .removed (none restored).", sources.len())
    }
}

// Send the occasion's whole .removed folder to the system trash (recoverable there).
fn trash_removed(c: &mut Cluster) -> String {
    let rem = c.dir.join(".removed");
    if !rem.exists() {
        return "Nothing removed for this occasion.".into();
    }
    let n = c.removed;
    let ok = Command::new("gio")
        .arg("trash")
        .arg(&rem)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok && fs::remove_dir_all(&rem).is_err() {
        return "Failed to trash .removed.".into();
    }
    c.removed = 0;
    format!("Trashed .removed ({n} shots) to system trash.")
}

// ---------- ui ----------

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(7),
        ])
        .split(f.area());

    let total: usize = app.clusters.iter().map(|c| c.files.len()).sum();
    let unit = if app.library { "folders" } else { "clusters" };
    let mut hdr = vec![
        Span::styled(" wpic ", Style::new().bg(Color::Cyan).fg(Color::Black).bold()),
        Span::raw(format!("  {} {} · {} files · ", app.clusters.len(), unit, total)),
    ];
    hdr.push(Span::styled(
        format!("★≥{} ", app.min_rating),
        Style::new().fg(Color::Yellow),
    ));
    hdr.push(Span::raw("([ ]) · "));
    if app.library {
        hdr.push(Span::styled("LIBRARY", Style::new().fg(Color::Cyan).bold()));
        hdr.push(Span::raw(format!("  {}  (↥ {})", app.pictures.display(), rclone_dest())));
    } else {
        let action = if app.do_move { "MOVE" } else { "COPY" };
        hdr.push(Span::raw("action="));
        hdr.push(Span::styled(
            action,
            Style::new().fg(if app.do_move { Color::Red } else { Color::Green }).bold(),
        ));
        hdr.push(Span::raw(format!("  ->  {}", app.pictures.display())));
    }
    let header = Paragraph::new(Line::from(hdr)).block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    let min = app.min_rating;
    let library = app.library;
    let items: Vec<ListItem> = app
        .clusters
        .iter()
        .map(|c| {
            let span = format!("{}  {}-{}", c.date_str(), fmt_time(c.start), fmt_time(c.end));
            let (mark, mstyle) = if c.skip {
                (if library { "····" } else { "STAY" }, Style::new().fg(Color::DarkGray))
            } else if desc_part(&c.target).trim().is_empty() {
                ("????", Style::new().fg(Color::Yellow).bold())
            } else if c.suggested {
                (" +> ", Style::new().fg(Color::Blue))
            } else {
                (" -> ", Style::new().fg(Color::Green))
            };
            let target = if c.skip {
                (if library { "(empty)" } else { "(won't move)" }).to_string()
            } else {
                c.target.trim_end().to_string()
            };
            let nstar = c.rated_count(min);
            let star = if nstar > 0 {
                Span::styled(format!("  ★{nstar}"), Style::new().fg(Color::Yellow))
            } else {
                Span::raw("")
            };
            let up = match c.up {
                UploadMode::No => Span::raw(""),
                UploadMode::All => Span::styled("  ↥ all", Style::new().fg(Color::Cyan)),
                UploadMode::Photos => Span::styled("  ↥ pics", Style::new().fg(Color::Cyan)),
                UploadMode::Rated => {
                    Span::styled(format!("  ↥ ★{nstar} pics"), Style::new().fg(Color::Cyan))
                }
            };
            let rem = if c.removed > 0 {
                Span::styled(format!("  ✗{}", c.removed), Style::new().fg(Color::Red))
            } else {
                Span::raw("")
            };
            let mov_span = if c.movie_count() > 0 {
                Span::styled(format!("{:>2}m ", c.movie_count()), Style::new().fg(Color::Yellow))
            } else {
                Span::raw("    ")
            };
            // green folder name once it's confirmed present on Drive
            let name_style = if c.on_drive == Some(true) && !c.skip {
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                mstyle.add_modifier(Modifier::BOLD)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", mark), mstyle),
                Span::styled(format!("{:>3}p ", c.pic_count()), Style::new().fg(Color::Magenta)),
                mov_span,
                Span::styled(format!("{:>9} ", human_size(c.total_size())), Style::new().fg(Color::Blue)),
                Span::raw(format!("{:<24} ", span)),
                Span::styled(target, name_style),
                star,
                up,
                rem,
            ]))
        })
        .collect();
    let title = if app.library {
        " ~/Pictures · ↵ cull · s starred · f rated≥bar(1-5) · m movies · w wcut · R removed · T trash✗ · u upload · C drive · r rename · x · q "
    } else {
        " occasions · ↵ cull · s starred · f rated≥bar(1-5) · m movies · w wcut · R removed · u upload · C drive · r name · d don't-move · a · x "
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::new().bg(Color::DarkGray).bold())
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut app.list);

    let detail = if let Some(c) = app.clusters.get(app.sel()) {
        let samples: Vec<String> = c
            .files
            .iter()
            .take(6)
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        let more = if c.files.len() > 6 {
            format!("  …(+{})", c.files.len() - 6)
        } else {
            String::new()
        };
        vec![
            Line::from(format!(
                "span: {} {} → {}    types: {}",
                c.date_str(),
                fmt_time(c.start),
                fmt_time(c.end),
                c.ext_summary()
            )),
            Line::from(format!("files: {}{}", samples.join(", "), more)),
        ]
    } else {
        vec![Line::from("no files")]
    };
    let dtitle = if app.status.is_empty() {
        " detail ".to_string()
    } else {
        format!(" detail · {} ", app.status)
    };
    let detail = Paragraph::new(detail)
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title(dtitle));
    f.render_widget(detail, chunks[2]);

    match app.mode {
        Mode::Edit => edit_popup(f, app),
        Mode::Confirm => confirm_popup(f, app),
        Mode::Running => running_popup(f, app),
        Mode::Done => done_popup(f, app),
        Mode::Browse => {}
    }

    if let Some(msg) = &app.notice {
        let area = centered(f.area(), 64, 5);
        f.render_widget(Clear, area);
        let p = Paragraph::new(msg.clone())
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" please wait "));
        f.render_widget(p, area);
    }
}

fn running_popup(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 72, 8);
    f.render_widget(Clear, area);
    let Some(p) = &app.progress else { return };
    let stage = p.stage.lock().map(|s| s.clone()).unwrap_or_default();
    let total = p.total.load(Ordering::Relaxed).max(1);
    let done = p.done.load(Ordering::Relaxed);
    let errors = p.errors.load(Ordering::Relaxed);
    let frac = p.frac.load(Ordering::Relaxed) as f64 / 10000.0; // progress within current unit
    let ratio = ((done as f64 + frac) / total as f64).clamp(0.0, 1.0);
    let cur = p.current.lock().map(|c| c.clone()).unwrap_or_default();

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(2)])
        .split(area);

    let block = Block::default().borders(Borders::ALL).title(" working… ");
    f.render_widget(block, area);

    let gauge = Gauge::default()
        .gauge_style(Style::new().fg(Color::Green))
        .ratio(ratio)
        .label(format!("{}/{}  {:.0}%  ({} err)", done, total, ratio * 100.0, errors));
    f.render_widget(gauge, inner[0]);

    f.render_widget(
        Paragraph::new(Span::styled(format!("{} — {} of {}", stage, done, total), Style::new().bold())),
        inner[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(cur, Style::new().fg(Color::DarkGray))).wrap(Wrap { trim: true }),
        inner[2],
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w.min(area.width), h.min(area.height))
}

fn edit_popup(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 70, 7);
    f.render_widget(Clear, area);
    let body = vec![
        Line::from("Target folder name under ~/Pictures (Enter=save, Esc=cancel):"),
        Line::from(""),
        Line::from(Span::styled(
            format!("{}_", app.edit_buf),
            Style::new().fg(Color::Yellow).bold(),
        )),
    ];
    let p = Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(" edit name "));
    f.render_widget(p, area);
}

fn confirm_popup(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 66, 12);
    f.render_widget(Clear, area);
    let up = app.upload_count();
    let mut body: Vec<Line> = Vec::new();

    if app.library {
        body.push(Line::from(Span::styled(
            format!(" About to upload {} folder(s) to {}.", up, rclone_dest()),
            Style::new().bold(),
        )));
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " Files stay in ~/Pictures; nothing is moved or deleted.",
            Style::new().fg(Color::Green),
        )));
    } else {
        let verb = if app.do_move { "MOVE" } else { "COPY" };
        let n: usize = app
            .clusters
            .iter()
            .filter(|c| !c.skip && !desc_part(&c.target).trim().is_empty())
            .map(|c| c.files.len())
            .sum();
        let groups = app.assigned_count();
        let unnamed = app
            .clusters
            .iter()
            .filter(|c| !c.skip && desc_part(&c.target).trim().is_empty())
            .count();
        body.push(Line::from(Span::styled(
            format!(" About to {} {} files into {} folders.", verb, n, groups),
            Style::new().bold(),
        )));
        body.push(Line::from(""));
        if unnamed > 0 {
            body.push(Line::from(Span::styled(
                format!(" {} unnamed cluster(s) left in place.", unnamed),
                Style::new().fg(Color::Yellow),
            )));
        }
        if up > 0 {
            body.push(Line::from(Span::styled(
                format!(" Then upload {} folder(s) to {}.", up, rclone_dest()),
                Style::new().fg(Color::Cyan),
            )));
        }
        if app.do_move {
            body.push(Line::from(Span::styled(
                " MOVE deletes originals from the SD card.",
                Style::new().fg(Color::Red),
            )));
        }
    }
    body.push(Line::from(""));
    body.push(Line::from("  y = confirm    n/Esc = cancel"));
    let p = Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(" confirm "));
    f.render_widget(p, area);
}

fn done_popup(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 76, 16);
    f.render_widget(Clear, area);
    let mut body: Vec<Line> = app.log.iter().map(|l| Line::from(l.clone())).collect();
    body.push(Line::from(""));
    body.push(Line::from(Span::styled(" press q to quit ", Style::new().fg(Color::Cyan))));
    let p = Paragraph::new(body)
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title(" result "));
    f.render_widget(p, area);
}

// Rename the selected library folder on disk and re-point its files.
fn rename_folder(app: &mut App, i: usize) {
    let newname = app.edit_buf.trim().to_string();
    let oldname = app.clusters.get(i).map(|c| c.target.clone()).unwrap_or_default();
    if newname.is_empty() || newname == oldname {
        return;
    }
    if newname.contains('/') {
        app.status = "rename: name can't contain '/'".into();
        return;
    }
    let oldp = app.pictures.join(&oldname);
    let newp = app.pictures.join(&newname);
    if newp.exists() {
        app.status = format!("rename skipped: '{}' already exists", newname);
        return;
    }
    if fs::rename(&oldp, &newp).is_err() {
        app.status = "rename failed".into();
        return;
    }
    if let Some(c) = app.clusters.get_mut(i) {
        for fitem in &mut c.files {
            if let Some(fname) = fitem.path.file_name() {
                fitem.path = newp.join(fname);
            }
        }
        c.target = newname.clone();
    }
    app.status = format!("renamed → {}", newname);
}

// Show a notice, launch imv on the selected occasion (blocks), then apply changes.
fn do_review(
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    only_rated: bool,
) -> io::Result<()> {
    let min = app.min_rating;
    let what = if only_rated { format!("shots rated ≥{min}") } else { "previews".to_string() };
    app.notice = Some(format!(
        "Generating {what} and launching imv…\n\
         In imv: 1-5 = rate (0 clears) · Delete = cull · u = undo last cull · arrows = navigate · q = back."
    ));
    term.draw(|f| ui(f, app))?;
    let i = app.sel();
    let msg = if let Some(c) = app.clusters.get_mut(i) {
        review_cluster(c, only_rated, min)
    } else {
        String::new()
    };
    app.notice = None;
    app.status = msg;
    Ok(())
}

fn do_movies(term: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    let movies = app.clusters.get(app.sel()).map(|c| c.movies()).unwrap_or_default();
    if movies.is_empty() {
        app.status = "No movies in this occasion.".into();
        return Ok(());
    }
    app.notice = Some(format!(
        "Playing {} movie(s) in mpv…  (< > = prev/next clip · q = back)",
        movies.len()
    ));
    term.draw(|f| ui(f, app))?;
    // --no-terminal: don't fight the TUI for the controlling terminal (control via mpv window)
    let _ = Command::new("mpv").arg("--no-terminal").args(&movies).status();
    app.notice = None;
    app.status = format!("Viewed {} movie(s).", movies.len());
    Ok(())
}

// Suspend the TUI and run wcut in the occasion's folder (it's a terminal program
// that operates on the .MOV files in its working directory), then restore.
fn do_wcut(term: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    let Some(c) = app.clusters.get(app.sel()) else {
        return Ok(());
    };
    if c.movies().is_empty() {
        app.status = "No movies to cut in this occasion.".into();
        return Ok(());
    }
    let dir = c.dir.clone();

    // hand the terminal over to wcut
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    let status = Command::new("wcut").current_dir(&dir).status();
    // take the terminal back
    enable_raw_mode()?;
    execute!(term.backend_mut(), EnterAlternateScreen)?;
    term.clear()?;

    app.status = match status {
        Ok(_) => format!("wcut finished in {}", dir.display()),
        Err(e) => format!("Failed to launch wcut: {e}"),
    };
    Ok(())
}

// View only starred shots (rated 1-5, ignoring the [ ] threshold bar).
fn do_starred(term: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    app.notice = Some(
        "Generating starred shots (rated 1-5) and launching imv…\n\
         In imv: 1-5 = rate (0 clears) · Delete = cull · u = undo last cull · arrows · q = back."
            .to_string(),
    );
    term.draw(|f| ui(f, app))?;
    let i = app.sel();
    let msg = if let Some(c) = app.clusters.get_mut(i) {
        review_cluster(c, true, 1) // only_rated, min=1 (any star)
    } else {
        String::new()
    };
    app.notice = None;
    app.status = msg;
    Ok(())
}

fn do_view_removed(term: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    let i = app.sel();
    let has = app
        .clusters
        .get(i)
        .map(|c| c.dir.join(".removed").exists())
        .unwrap_or(false);
    if !has {
        app.status = "Nothing removed for this occasion.".into();
        return Ok(());
    }
    app.notice = Some(
        "Opening removed shots in imv…\nEnter = restore current shot · q = close.".to_string(),
    );
    term.draw(|f| ui(f, app))?;
    let msg = if let Some(c) = app.clusters.get_mut(i) {
        view_removed(c)
    } else {
        String::new()
    };
    app.notice = None;
    app.status = msg;
    Ok(())
}

// ---------- main loop ----------

fn run(term: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        // apply the background Drive check once its result lands
        if !app.drive_applied {
            let ready = app.drive_check.lock().ok().and_then(|g| g.clone());
            if let Some(res) = ready {
                app.drive_applied = true;
                match res {
                    Ok(listed) => {
                        apply_drive_listing(app, &listed);
                    }
                    Err(_) => {} // offline / not configured: leave markers unknown
                }
            }
        }

        term.draw(|f| ui(f, app))?;

        // detect worker completion and surface the result screen
        if app.mode == Mode::Running {
            if let Some(p) = &app.progress {
                if p.finished.load(Ordering::Relaxed) {
                    app.log = p.log.lock().map(|l| l.clone()).unwrap_or_default();
                    app.mode = Mode::Done;
                    continue;
                }
            }
        }

        // poll faster while working so the gauge animates smoothly
        let timeout = if app.mode == Mode::Running { 80 } else { 200 };
        if !event::poll(Duration::from_millis(timeout))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match app.mode {
            Mode::Browse => match k.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = (app.sel() + 1).min(app.clusters.len().saturating_sub(1));
                    app.list.select(Some(i));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = app.sel().saturating_sub(1);
                    app.list.select(Some(i));
                }
                KeyCode::Char('e') | KeyCode::Char('r') => {
                    if let Some(c) = app.clusters.get(app.sel()) {
                        app.edit_buf = c.target.clone();
                        app.mode = Mode::Edit;
                    }
                }
                KeyCode::Char('v') | KeyCode::Enter => do_review(term, app, false)?,
                KeyCode::Char('f') => do_review(term, app, true)?,
                KeyCode::Char('m') => do_movies(term, app)?,
                KeyCode::Char('w') => do_wcut(term, app)?,
                KeyCode::Char('C') => {
                    app.notice = Some(format!("Checking {} on Drive…", rclone_dest()));
                    term.draw(|f| ui(f, app))?;
                    app.status = check_drive(app);
                    app.notice = None;
                }
                KeyCode::Char('R') => do_view_removed(term, app)?,
                KeyCode::Char('T') => {
                    let i = app.sel();
                    let msg = if let Some(c) = app.clusters.get_mut(i) {
                        trash_removed(c)
                    } else {
                        String::new()
                    };
                    app.status = msg;
                }
                KeyCode::Char('u') => {
                    let i = app.sel();
                    let min = app.min_rating;
                    if let Some(c) = app.clusters.get_mut(i) {
                        c.up = match c.up {
                            UploadMode::No => UploadMode::All,
                            UploadMode::All => UploadMode::Photos,
                            UploadMode::Photos if c.rated_count(min) > 0 => UploadMode::Rated,
                            UploadMode::Photos => UploadMode::No,
                            UploadMode::Rated => UploadMode::No,
                        };
                    }
                }
                KeyCode::Char('[') => app.min_rating = app.min_rating.saturating_sub(1).max(1),
                KeyCode::Char(']') => app.min_rating = (app.min_rating + 1).min(5),
                KeyCode::Char(d @ '1'..='5') => app.min_rating = d as u8 - b'0', // set the f bar directly

                KeyCode::Char('s') | KeyCode::Char('S') => do_starred(term, app)?,
                KeyCode::Char('d') => {
                    // "don't move" — card mode only (skip is meaningless in library)
                    if !app.library {
                        let i = app.sel();
                        if let Some(c) = app.clusters.get_mut(i) {
                            c.skip = !c.skip;
                        }
                    }
                }
                KeyCode::Char('a') => {
                    if !app.library {
                        app.do_move = !app.do_move;
                    }
                }
                KeyCode::Char('x') => {
                    let ready = if app.library {
                        app.upload_count() > 0
                    } else {
                        app.assigned_count() > 0
                    };
                    if ready {
                        app.mode = Mode::Confirm;
                    }
                }
                _ => {}
            },
            Mode::Edit => match k.code {
                KeyCode::Esc => app.mode = Mode::Browse,
                KeyCode::Enter => {
                    let i = app.sel();
                    if app.library {
                        rename_folder(app, i);
                    } else if let Some(c) = app.clusters.get_mut(i) {
                        c.target = app.edit_buf.clone();
                        c.suggested = false;
                        c.skip = false;
                    }
                    app.mode = Mode::Browse;
                }
                KeyCode::Backspace => {
                    app.edit_buf.pop();
                }
                KeyCode::Char(ch) => app.edit_buf.push(ch),
                _ => {}
            },
            Mode::Confirm => match k.code {
                KeyCode::Char('y') => start_execute(app),
                KeyCode::Char('n') | KeyCode::Esc => app.mode = Mode::Browse,
                _ => {}
            },
            Mode::Running => {} // ignore input while the worker runs
            Mode::Done => match k.code {
                KeyCode::Char('q') | KeyCode::Enter | KeyCode::Esc => return Ok(()),
                _ => {}
            },
        }
    }
}

fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .and_then(|h| Path::new(&h).file_name().map(|n| n.to_string_lossy().to_string()))
        })
        .unwrap_or_else(|| "root".into())
}

// Auto-detect a mounted camera card: the first <mount>/DCIM under the user's
// removable-media dirs, regardless of the card's volume label. Returns a
// non-existent path when nothing is mounted (→ falls back to library mode).
fn default_dcim() -> PathBuf {
    let user = username();
    for base in [format!("/run/media/{user}"), format!("/media/{user}")] {
        if let Ok(rd) = fs::read_dir(&base) {
            for e in rd.flatten() {
                let dcim = e.path().join("DCIM");
                if dcim.is_dir() {
                    return dcim;
                }
            }
        }
    }
    PathBuf::from(format!("/run/media/{user}/NO_CARD/DCIM"))
}

fn detect_offset() -> i64 {
    // std has no local-time support; ask the system for its UTC offset once.
    use std::process::Command;
    if let Ok(out) = Command::new("date").arg("+%z").output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            let s = s.trim();
            if s.len() == 5 {
                let sign = if &s[0..1] == "-" { -1 } else { 1 };
                if let (Ok(h), Ok(m)) = (s[1..3].parse::<i64>(), s[3..5].parse::<i64>()) {
                    return sign * (h * 3600 + m * 60);
                }
            }
        }
    }
    0
}

fn main() -> io::Result<()> {
    let pictures = config().pictures.clone();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let list_only = args.iter().any(|a| a == "--list");
    let force_library = args.iter().any(|a| a == "--library" || a == "-l");
    let dcim = args
        .iter()
        .find(|a| !a.starts_with("-"))
        .map(PathBuf::from)
        .unwrap_or_else(default_dcim);

    unsafe {
        LOCAL_OFFSET = detect_offset();
    }

    // diagnostic: print our EXIF orientation read for given files
    if args.iter().any(|a| a == "--orient") {
        for f in args.iter().filter(|a| !a.starts_with('-')) {
            println!("{}\t{}", media_orientation(Path::new(f)), f);
        }
        return Ok(());
    }

    // Prefer the card; fall back to managing ~/Pictures when there's no card.
    let mut library = force_library;
    let mut clusters: Vec<Cluster> = Vec::new();
    if !force_library {
        if dcim.exists() {
            eprintln!("Scanning card {} …", dcim.display());
            let files = scan(&dcim)?;
            if files.is_empty() {
                eprintln!("Card has no images — opening ~/Pictures library instead.");
                library = true;
            } else {
                clusters = cluster(files, &pictures);
            }
        } else {
            eprintln!("No card at {} — opening ~/Pictures library instead.", dcim.display());
            library = true;
        }
    }
    if library {
        eprintln!("Scanning library {} …", pictures.display());
        clusters = scan_library(&pictures);
        if clusters.is_empty() {
            eprintln!("No photo folders found under {}.", pictures.display());
            std::process::exit(0);
        }
    }

    if list_only {
        if library {
            println!("{} folders under {}:\n", clusters.len(), pictures.display());
        } else {
            println!("{} clusters ({}h gap):\n", clusters.len(), config().gap_hours);
        }
        for (i, c) in clusters.iter().enumerate() {
            let tag = if c.suggested { "[existing]" } else { "[new]" };
            let stars = if c.rated_count(1) > 0 {
                format!("  ★{}", c.rated_count(1))
            } else {
                String::new()
            };
            let rem = if c.removed > 0 { format!("  ✗{}", c.removed) } else { String::new() };
            println!(
                "{:>3}. {} {}-{}  {:>3} pics {:>2} mov  {:>9}  {:<10} {}{}{}",
                i + 1,
                c.date_str(),
                fmt_time(c.start),
                fmt_time(c.end),
                c.pic_count(),
                c.movie_count(),
                human_size(c.total_size()),
                tag,
                c.target.trim_end(),
                stars,
                rem
            );
        }
        return Ok(());
    }

    let mut app = App {
        pictures,
        clusters,
        list: {
            let mut s = ListState::default();
            s.select(Some(0));
            s
        },
        mode: Mode::Browse,
        edit_buf: String::new(),
        do_move: true, // default MOVE; press 'a' for COPY
        library,
        min_rating: 1,
        log: Vec::new(),
        progress: None,
        notice: None,
        status: String::new(),
        drive_check: Arc::new(Mutex::new(None)),
        drive_applied: false,
    };

    // kick off a background Drive presence check so ☁ markers appear after launch
    {
        let slot = app.drive_check.clone();
        let dest = rclone_dest();
        std::thread::spawn(move || {
            let res = rclone_list_dirs(&dest);
            if let Ok(mut g) = slot.lock() {
                *g = Some(res);
            }
        });
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let res = run(&mut term, &mut app);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}
