//! The file browser in the viewer (owner only): list a folder under the home folder, fetch
//! a file, upload into a folder, make folders, rename and move to the Trash, and small
//! previews of images and PDFs. Paths from the viewer are relative to the home folder and
//! must stay inside it: no `..`, and no symlink that leads out.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

/// Folders listed in one go (a folder with more shows the first ones and says so).
const MAX_ENTRIES: usize = 5000;
/// Previews are this many pixels on their longest side.
#[cfg(target_os = "macos")]
const THUMB_SIZE: u32 = 160;
/// Drag-out links (`/files/raw`) work this long after the listing that made them.
const TICKET_TTL: Duration = Duration::from_secs(15 * 60);

/// Where browsing starts and must stay: the home folder (tests: `GLASS_FILES_ROOT`).
pub fn root() -> PathBuf {
    if crate::testmode::enabled()
        && let Some(r) = std::env::var_os("GLASS_FILES_ROOT")
    {
        return PathBuf::from(r);
    }
    crate::home()
}

/// A path from the viewer ("Documents/Taxes"), checked to be inside `root` once symlinks
/// are followed. It must exist.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf> {
    let joined = join(root, rel)?;
    let real = joined.canonicalize().with_context(|| format!("{rel} isn't there"))?;
    let root = root.canonicalize().context("the home folder isn't there")?;
    if !real.starts_with(&root) {
        bail!("that's outside your home folder");
    }
    Ok(real)
}

/// `rel` under `root` without touching the disk: only plain names, no `..` or absolute parts.
fn join(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for c in Path::new(rel.trim_matches('/')).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => bail!("not a path inside your home folder"),
        }
    }
    Ok(out)
}

/// A new file or folder name: one plain component.
pub fn check_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) || name.len() > 255 {
        bail!("that name can't be used");
    }
    Ok(name)
}

/// The viewer-facing path of `real` (relative to the root, `/`-separated).
pub fn relative(root: &Path, real: &Path) -> String {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    real.strip_prefix(&root)
        .map(|p| p.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Unix seconds.
    pub modified: u64,
    /// "image", "pdf", "video", "audio", "text", "archive", "app" or "" (for icons and
    /// whether a preview is worth asking for).
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    /// Relative to the home folder ("" is home itself).
    pub path: String,
    pub entries: Vec<Entry>,
    /// More entries than were sent.
    pub truncated: bool,
}

pub fn list(root: &Path, rel: &str, hidden: bool) -> Result<Listing> {
    let dir = resolve(root, rel)?;
    if !dir.is_dir() {
        bail!("{rel} isn't a folder");
    }
    let mut entries = Vec::new();
    let mut truncated = false;
    for e in std::fs::read_dir(&dir).with_context(|| format!("can't open {}", dir.display()))? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if !hidden && name.starts_with('.') {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        // Follow symlinks for what they point at, but list them where they are.
        let Ok(meta) = std::fs::metadata(e.path()) else { continue };
        let dir = meta.is_dir() && !name.ends_with(".app");
        entries.push(Entry {
            kind: if name.ends_with(".app") { "app" } else if dir { "" } else { kind(&name) },
            size: if dir { 0 } else { meta.len() },
            modified: meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()),
            name,
            dir,
        });
    }
    // Folders first, then by name (case-insensitive), like Finder.
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(Listing { path: relative(root, &dir), entries, truncated })
}

fn kind(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "heif" | "tif" | "tiff" | "bmp" | "svg" => "image",
        "pdf" => "pdf",
        "mp4" | "mov" | "m4v" | "webm" | "mkv" | "avi" => "video",
        "mp3" | "m4a" | "wav" | "aiff" | "flac" | "aac" | "ogg" => "audio",
        "txt" | "md" | "rtf" | "csv" | "json" | "log" | "rs" | "js" | "ts" | "py" | "html" | "css" | "sh" | "toml" | "yaml" | "yml" => "text",
        "zip" | "tar" | "gz" | "tgz" | "dmg" | "7z" | "rar" | "xz" => "archive",
        _ => "",
    }
}

pub fn mkdir(root: &Path, parent: &str, name: &str) -> Result<PathBuf> {
    let dir = resolve(root, parent)?;
    let path = dir.join(check_name(name)?);
    if path.exists() {
        bail!("{name} already exists");
    }
    std::fs::create_dir(&path).with_context(|| format!("can't make {name}"))?;
    Ok(path)
}

pub fn rename(root: &Path, rel: &str, name: &str) -> Result<PathBuf> {
    let from = resolve(root, rel)?;
    if from == root.canonicalize()? {
        bail!("the home folder can't be renamed");
    }
    let to = from.parent().context("no parent")?.join(check_name(name)?);
    if to.exists() {
        bail!("{name} already exists");
    }
    std::fs::rename(&from, &to).with_context(|| format!("can't rename {rel}"))?;
    Ok(to)
}

/// Moves to the Trash (never deletes outright).
pub fn trash(root: &Path, rel: &str) -> Result<()> {
    let path = resolve(root, rel)?;
    if path == root.canonicalize()? {
        bail!("the home folder can't go in the Trash");
    }
    if crate::testmode::enabled() {
        // Tests: a .Trash folder inside the test root, never the real one.
        let bin = root.join(".Trash");
        std::fs::create_dir_all(&bin)?;
        let name = path.file_name().context("no name")?;
        std::fs::rename(&path, crate::control::unique_path(&bin, &name.to_string_lossy()))?;
        return Ok(());
    }
    move_to_trash(&path)
}

#[cfg(target_os = "macos")]
fn move_to_trash(path: &Path) -> Result<()> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    NSFileManager::defaultManager().trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))
}

#[cfg(not(target_os = "macos"))]
fn move_to_trash(path: &Path) -> Result<()> {
    let status = std::process::Command::new("gio").arg("trash").arg(path).status().context("gio trash")?;
    anyhow::ensure!(status.success(), "couldn't move it to the Trash");
    Ok(())
}

/// A small PNG preview of an image or PDF, as a data URL; `None` when there isn't one.
pub fn thumbnail(root: &Path, rel: &str) -> Result<Option<String>> {
    let path = resolve(root, rel)?;
    if path.is_dir() {
        return Ok(None);
    }
    let modified = std::fs::metadata(&path)?.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let cache = crate::config_dir().join("thumbs");
    std::fs::create_dir_all(&cache)?;
    let key = {
        let d = ring::digest::digest(&ring::digest::SHA256, format!("{}\0{modified}", path.display()).as_bytes());
        d.as_ref()[..12].iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let out = cache.join(format!("{key}.png"));
    if !out.exists() && !make_thumbnail(&path, &out)? {
        return Ok(None);
    }
    let png = std::fs::read(&out)?;
    use base64::Engine;
    Ok(Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))))
}

/// Quick Look's thumbnailer (images, PDFs, videos, most documents).
#[cfg(target_os = "macos")]
fn make_thumbnail(path: &Path, out: &Path) -> Result<bool> {
    let tmp = tempdir()?;
    let status = std::process::Command::new("qlmanage")
        .args(["-t", "-s", &THUMB_SIZE.to_string(), "-o"])
        .arg(&tmp)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    let made = std::fs::read_dir(&tmp)?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "png"));
    let ok = status.success() && made.as_ref().is_some_and(|m| std::fs::rename(m, out).is_ok());
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(ok)
}

#[cfg(not(target_os = "macos"))]
fn make_thumbnail(_path: &Path, _out: &Path) -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn tempdir() -> Result<PathBuf> {
    let mut b = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).map_err(|_| anyhow::anyhow!("random"))?;
    let d = std::env::temp_dir().join(format!("glass-thumb-{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

// ---- Drag-out links: `/files/raw?t=<ticket>&path=…` ----

static TICKETS: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// A ticket for downloading by URL (dragging a file out of the viewer onto the desktop),
/// handed to an owner's session with each listing.
pub fn ticket() -> String {
    let mut b = [0u8; 18];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).expect("random");
    let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let mut map = TICKETS.lock().unwrap();
    let map = map.get_or_insert_with(HashMap::new);
    map.retain(|_, at| at.elapsed() < TICKET_TTL);
    map.insert(t.clone(), Instant::now());
    t
}

fn ticket_ok(t: &str) -> bool {
    TICKETS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.iter().find(|(k, _)| crate::control::token_matches(k, t)).map(|(_, at)| at.elapsed() < TICKET_TTL))
        .unwrap_or(false)
}

#[derive(serde::Deserialize)]
struct RawQuery {
    t: String,
    path: String,
}

pub fn routes() -> axum::Router {
    axum::Router::new().route("/files/raw", axum::routing::get(raw))
}

async fn raw(axum::extract::Query(q): axum::extract::Query<RawQuery>) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    if !ticket_ok(&q.t) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(path) = resolve(&root(), &q.path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(file) = tokio::fs::File::open(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if path.is_dir() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = axum::body::Body::from_stream(tokio_util_stream(file));
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename*=UTF-8''{}", encode(&name))),
        ],
        body,
    )
        .into_response()
}

/// A file as a stream of chunks.
fn tokio_util_stream(file: tokio::fs::File) -> impl futures_util::Stream<Item = std::io::Result<bytes::Bytes>> {
    futures_util::stream::unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 256 << 10];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(bytes::Bytes::from(buf)), file))
            }
            Err(e) => Some((Err(e), file)),
        }
    })
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("glass-browse-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(d.join("Documents/Taxes")).unwrap();
        std::fs::write(d.join("Documents/a.png"), b"x").unwrap();
        std::fs::write(d.join(".hidden"), b"x").unwrap();
        d
    }

    fn rand_suffix() -> u64 {
        std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }

    #[test]
    fn stays_inside_the_root() {
        let root = tmp();
        assert!(resolve(&root, "Documents/Taxes").is_ok());
        assert!(resolve(&root, "").is_ok());
        assert!(resolve(&root, "../").is_err());
        assert!(resolve(&root, "Documents/../../etc").is_err());
        // Absolute paths are taken as inside the root ("/etc" is <root>/etc, not there).
        assert!(resolve(&root, "/etc").is_err());
        assert_eq!(resolve(&root, "/Documents").unwrap(), root.join("Documents").canonicalize().unwrap());
        assert!(resolve(&root, "nope").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn symlinks_out_are_refused() {
        let root = tmp();
        let outside = std::env::temp_dir().join(format!("glass-outside-{}", rand_suffix()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        std::os::unix::fs::symlink(root.join("Documents"), root.join("docs")).unwrap();
        assert!(resolve(&root, "escape").is_err());
        assert!(resolve(&root, "docs/Taxes").is_ok());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn lists_folders_first_without_hidden_files() {
        let root = tmp();
        let l = list(&root, "", false).unwrap();
        assert_eq!(l.path, "");
        assert_eq!(l.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["Documents"]);
        assert!(list(&root, "", true).unwrap().entries.iter().any(|e| e.name == ".hidden"));
        let docs = list(&root, "Documents", false).unwrap();
        assert_eq!(docs.path, "Documents");
        assert_eq!(docs.entries[0], Entry { name: "Taxes".into(), dir: true, size: 0, modified: docs.entries[0].modified, kind: "" });
        assert_eq!(docs.entries[1].kind, "image");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_are_one_plain_component() {
        assert!(check_name("Report.pdf").is_ok());
        for bad in ["", ".", "..", "a/b", "a\\b", "x\0"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        let root = tmp();
        assert!(mkdir(&root, "Documents", "../x").is_err());
        assert!(mkdir(&root, "Documents", "New").is_ok());
        assert!(rename(&root, "Documents/New", "Old").is_ok());
        assert!(rename(&root, "", "x").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
