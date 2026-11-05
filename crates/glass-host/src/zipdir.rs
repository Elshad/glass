//! A folder from the file browser as a zip, built while it downloads: nothing is staged on
//! disk and memory stays at a few chunks however big the folder is. Symlinks are left out
//! (they could lead outside the home folder, or loop), and every file and folder is checked
//! to still be inside the root before it's read.
//!
//! Adapted from voxl (MIT, github.com/tembo/voxl)

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use tokio::sync::mpsc;

const CHUNK: usize = 256 << 10;

/// The zip's file name for `dir`: "Photos.zip".
pub fn name(dir: &Path) -> String {
    let base = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{}.zip", if base.is_empty() { "folder" } else { &base })
}

/// Zips `dir` (already resolved inside `root`) on a blocking thread, handing out chunks as
/// they're made. The thread waits while the reader is behind; dropping the receiver stops it.
/// A failure part way through comes out as an `Err` chunk, after which the zip is incomplete.
pub fn stream(root: PathBuf, dir: PathBuf) -> mpsc::Receiver<std::io::Result<Bytes>> {
    let (tx, rx) = mpsc::channel(4);
    tokio::task::spawn_blocking(move || {
        let top = name(&dir);
        let top = top.trim_end_matches(".zip");
        let out = ChunkWriter { tx: tx.clone(), buf: Vec::with_capacity(CHUNK) };
        if let Err(e) = write_zip(&root, &dir, top, out) {
            tracing::warn!("zipping {}: {e}", dir.display());
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
        }
    });
    rx
}

/// Hands the zip out in chunks; blocks while the reader catches up.
struct ChunkWriter {
    tx: mpsc::Sender<std::io::Result<Bytes>>,
    buf: Vec<u8>,
}

impl ChunkWriter {
    fn send(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        self.tx
            .blocking_send(Ok(chunk.into()))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "download cancelled"))
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.send()?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.send()
    }
}

/// Formats that are already compressed: stored as they are rather than deflated again.
const COMPRESSED: &[&str] = &[
    "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "br", "jar", "whl", "apk", "dmg", "iso",
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "mp3", "m4a", "aac", "ogg", "opus",
    "flac", "mp4", "m4v", "mov", "mkv", "webm", "avi", "woff", "woff2", "pdf", "docx", "xlsx",
    "pptx",
];

/// Zip time (local fields, no zone) for a modification time, read as UTC.
fn zip_time(t: std::time::SystemTime) -> Option<zip::DateTime> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
    zip::DateTime::from_date_and_time(year, month, day, (rem / 3600) as u8, (rem % 3600 / 60) as u8, (rem % 60) as u8).ok()
}

/// Still inside `root` once symlinks are followed (something may have been swapped for a
/// link since the folder was listed).
fn inside(root: &Path, path: &Path) -> bool {
    path.canonicalize().is_ok_and(|p| p.starts_with(root))
}

/// Writes `dir` as a zip whose entries all sit under `top/`.
fn write_zip<W: Write>(root: &Path, dir: &Path, top: &str, out: W) -> zip::result::ZipResult<W> {
    use zip::write::SimpleFileOptions;
    let root = root.canonicalize()?;
    if !inside(&root, dir) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "outside the home folder").into());
    }
    let mut z = zip::ZipWriter::new_stream(out);
    let base = SimpleFileOptions::default();
    let mut todo = vec![(dir.to_path_buf(), top.to_string())];
    while let Some((path, rel)) = todo.pop() {
        z.add_directory(format!("{rel}/"), base)?;
        let mut entries: Vec<_> = match std::fs::read_dir(&path) {
            Ok(rd) => rd.filter_map(Result::ok).collect(),
            // Unreadable folders are left empty rather than failing the whole download.
            Err(_) => continue,
        };
        entries.sort_by_key(|e| e.file_name());
        // Pushed in reverse so folders come out in name order.
        let mut dirs = Vec::new();
        for e in entries {
            // Not followed: a symlink (or a Windows junction) is neither a file nor a folder here.
            let Ok(md) = e.path().symlink_metadata() else { continue };
            if md.file_type().is_symlink() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = format!("{rel}/{name}");
            if md.is_dir() {
                if inside(&root, &e.path()) {
                    dirs.push((e.path(), rel));
                }
                continue;
            }
            if !md.is_file() || !inside(&root, &e.path()) {
                continue;
            }
            let Ok(mut file) = std::fs::File::open(e.path()) else { continue };
            let stored = Path::new(&name)
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| COMPRESSED.contains(&x.to_ascii_lowercase().as_str()));
            let mut opts = base.large_file(md.len() >= u32::MAX as u64);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                opts = opts.unix_permissions(md.permissions().mode() & 0o777);
            }
            opts = if stored {
                opts.compression_method(zip::CompressionMethod::Stored)
            } else {
                // Fastest level: the CPU is busy streaming the desktop.
                opts.compression_method(zip::CompressionMethod::Deflated).compression_level(Some(1))
            };
            if let Some(t) = md.modified().ok().and_then(zip_time) {
                opts = opts.last_modified_time(t);
            }
            z.start_file(rel, opts)?;
            std::io::copy(&mut file, &mut z)?;
        }
        todo.extend(dirs.into_iter().rev());
    }
    let mut out = z.finish()?.into_inner();
    out.flush()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn tmp() -> PathBuf {
        // Tests run in parallel and the clock may be coarse: a counter keeps the names apart.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("glass-zipdir-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn zip_of(root: &Path, dir: &Path) -> zip::ZipArchive<std::io::Cursor<Vec<u8>>> {
        let bytes = write_zip(root, dir, &name(dir).replace(".zip", ""), Vec::new()).unwrap();
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap()
    }

    fn names(z: &zip::ZipArchive<std::io::Cursor<Vec<u8>>>) -> Vec<String> {
        let mut v: Vec<String> = z.file_names().map(str::to_string).collect();
        v.sort();
        v
    }

    #[test]
    fn round_trips_a_folder() {
        let root = tmp();
        let dir = root.join("Photos");
        std::fs::create_dir_all(dir.join("2024/empty")).unwrap();
        std::fs::write(dir.join("notes.txt"), "hello ".repeat(1000)).unwrap();
        std::fs::write(dir.join("2024/a.jpg"), [1u8, 2, 3]).unwrap();
        let big: Vec<u8> = (0..(3 * CHUNK)).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(dir.join("2024/big.bin"), &big).unwrap();

        let mut z = zip_of(&root, &dir);
        assert_eq!(
            names(&z),
            ["Photos/", "Photos/2024/", "Photos/2024/a.jpg", "Photos/2024/big.bin", "Photos/2024/empty/", "Photos/notes.txt"]
        );
        let read = |z: &mut zip::ZipArchive<_>, n: &str| {
            let mut v = Vec::new();
            z.by_name(n).unwrap().read_to_end(&mut v).unwrap();
            v
        };
        assert_eq!(read(&mut z, "Photos/notes.txt"), "hello ".repeat(1000).into_bytes());
        assert_eq!(read(&mut z, "Photos/2024/a.jpg"), [1, 2, 3]);
        assert_eq!(read(&mut z, "Photos/2024/big.bin"), big);
        assert_eq!(z.by_name("Photos/2024/a.jpg").unwrap().compression(), zip::CompressionMethod::Stored);
        assert_eq!(z.by_name("Photos/notes.txt").unwrap().compression(), zip::CompressionMethod::Deflated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn skips_symlinks() {
        let root = tmp();
        let outside = tmp();
        std::fs::write(outside.join("secret"), "x").unwrap();
        let dir = root.join("Shared");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("real.txt"), "ok").unwrap();
        std::fs::write(root.join("sibling.txt"), "y").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.join("file-link")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(root.join("sibling.txt"), dir.join("inside-link")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();

        let z = zip_of(&root, &dir);
        assert_eq!(names(&z), ["Shared/", "Shared/real.txt"]);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn stays_inside_the_root() {
        let root = tmp();
        let other = tmp();
        std::fs::write(other.join("x"), "x").unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        // A folder outside the root is refused outright.
        assert!(write_zip(&home, &other, "other", Vec::new()).is_err());
        assert!(write_zip(&home, &root, "root", Vec::new()).is_err());
        // Entry names never climb out of the top folder.
        std::fs::create_dir_all(home.join("a/b")).unwrap();
        std::fs::write(home.join("a/b/c.txt"), "c").unwrap();
        let z = zip_of(&home, &home.join("a"));
        for n in names(&z) {
            assert!(n.starts_with("a/") && !n.split('/').any(|p| p == ".."), "{n}");
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn streams_in_chunks() {
        let root = tmp();
        let dir = root.join("d");
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..(5 * CHUNK)).map(|i| (i * 13 % 251) as u8).collect();
        std::fs::write(dir.join("f.mp4"), &data).unwrap();
        let mut rx = stream(root.clone(), dir);
        let mut all = Vec::new();
        let mut chunks = 0;
        while let Some(c) = rx.recv().await {
            let c = c.unwrap();
            all.extend_from_slice(&c);
            chunks += 1;
        }
        assert!(chunks > 1);
        // The viewer takes a streamed zip as whole when it ends with the end record.
        assert_eq!(all[all.len() - 22..][..4], [0x50, 0x4b, 5, 6]);
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(all)).unwrap();
        let mut got = Vec::new();
        z.by_name("d/f.mp4").unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, data);
        let _ = std::fs::remove_dir_all(&root);
    }
}
