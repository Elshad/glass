//! File transfer over WebTransport unidirectional streams, in both directions.
//! Each stream: `STREAM_FILE` tag, u32 LE header length, JSON [`FileHeader`], file bytes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;
use wtransport::{Connection, RecvStream};

use crate::control::{FileHeader, STREAM_FILE, unique_path};

const MAX_HEADER: usize = 64 << 10;
const CHUNK: usize = 256 << 10;

static DOWNLOADS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn set_downloads_dir(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = DOWNLOADS.set(dir);
}

pub fn downloads_dir() -> PathBuf {
    DOWNLOADS
        .get()
        .cloned()
        .unwrap_or_else(|| crate::home().join("Downloads"))
}

/// Lets the remote user pick files on the host to send to their device.
pub async fn pick() -> Result<Vec<PathBuf>> {
    if crate::testmode::enabled() {
        return Ok(crate::testmode::picked_files());
    }
    pick_native().await
}

/// Receives one uploaded file (tag already consumed) into ~/Downloads.
/// Writes to a temporary name first so a cancelled upload never looks complete.
pub async fn receive(stream: &mut RecvStream, owner: bool) -> Result<(String, PathBuf)> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_HEADER {
        bail!("file header too large");
    }
    let mut header = vec![0u8; len];
    stream.read_exact(&mut header).await?;
    let header: FileHeader = serde_json::from_slice(&header)?;

    // A file from a dropped folder keeps its place in it ("Photos/2024/a.jpg").
    // The file browser uploads into the folder it shows (owners only, inside home).
    let base = match header.dir.as_deref() {
        Some(dir) if owner => crate::browse::resolve(&crate::browse::root(), dir)?,
        Some(_) => anyhow::bail!("uploading into a folder is for the computer's owner"),
        None => downloads_dir(),
    };
    let (dir, name) = place(&base, &header.name);
    tokio::fs::create_dir_all(&dir).await?;
    let dest = unique_path(&dir, &name);
    let partial = dest.with_extension(format!(
        "{}glass-partial",
        dest.extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut received = 0u64;
    let result: Result<()> = async {
        while received < header.size {
            let want = CHUNK.min((header.size - received) as usize);
            let n = stream
                .read(&mut buf[..want])
                .await?
                .context("upload cut short")?;
            file.write_all(&buf[..n]).await?;
            received += n as u64;
        }
        file.flush().await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(e);
    }
    tokio::fs::rename(&partial, &dest).await?;
    let name = dest.file_name().unwrap().to_string_lossy().into_owned();
    info!("received file {} ({received} bytes)", dest.display());
    Ok((name, dest))
}

/// Where an uploaded `name` goes under `base`: its folders (safe ones only: no `..`, no
/// hidden names, at most 16 deep) and its file name.
fn place(base: &Path, name: &str) -> (PathBuf, String) {
    let mut parts: Vec<&str> = name
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != "." && *p != ".." && !p.starts_with('.'))
        .collect();
    let file = parts.pop().unwrap_or("file").to_string();
    let mut dir = base.to_path_buf();
    for p in parts.into_iter().take(16) {
        dir.push(p);
    }
    (dir, file)
}

/// Shows a native file chooser on the host's screen (visible to the remote user).
#[cfg(target_os = "macos")]
async fn pick_native() -> Result<Vec<PathBuf>> {
    let script = r#"set picked to choose file with prompt "Send to Glass client" with multiple selections allowed
set out to ""
repeat with f in picked
    set out to out & POSIX path of f & linefeed
end repeat
return out"#;
    let output = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await?;
    if !output.status.success() {
        // -128 is "User canceled".
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Shows the desktop's file chooser through the portal.
#[cfg(target_os = "linux")]
async fn pick_native() -> Result<Vec<PathBuf>> {
    use ashpd::desktop::file_chooser::SelectedFiles;
    let response = SelectedFiles::open_file()
        .title("Send to Glass client")
        .multiple(true)
        .send()
        .await?
        .response();
    // Cancelled dialogs come back as an error response.
    let Ok(files) = response else {
        return Ok(Vec::new());
    };
    Ok(files
        .uris()
        .iter()
        .filter_map(|u| file_uri_to_path(u.as_str()))
        .collect())
}

/// Shows the standard Open dialog on the host's screen.
#[cfg(windows)]
async fn pick_native() -> Result<Vec<PathBuf>> {
    let script = "[Console]::OutputEncoding = [Text.Encoding]::UTF8; Add-Type -AssemblyName System.Windows.Forms; \
        $d = New-Object System.Windows.Forms.OpenFileDialog; $d.Title = 'Send to Glass client'; $d.Multiselect = $true; \
        if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { $d.FileNames }";
    let mut cmd = crate::platform::powershell(script);
    cmd.arg("-Sta");
    let output = tokio::process::Command::from(cmd).output().await?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// `file:///home/me/My%20File.txt` → `/home/me/My File.txt`.
#[cfg(any(target_os = "linux", all(unix, test)))]
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_stay_inside_downloads() {
        let base = Path::new("/d");
        assert_eq!(place(base, "a.txt"), (PathBuf::from("/d"), "a.txt".into()));
        assert_eq!(place(base, "Photos/2024/a.jpg"), (PathBuf::from("/d/Photos/2024"), "a.jpg".into()));
        assert_eq!(place(base, "../../etc/passwd"), (PathBuf::from("/d/etc"), "passwd".into()));
        assert_eq!(place(base, "/abs/.ssh/x"), (PathBuf::from("/d/abs"), "x".into()));
        assert_eq!(place(base, "dir/"), (PathBuf::from("/d"), "dir".into()));
    }

    #[test]
    #[cfg(unix)]
    fn decodes_file_uris() {
        assert_eq!(
            file_uri_to_path("file:///home/me/My%20File.txt"),
            Some(PathBuf::from("/home/me/My File.txt"))
        );
        assert_eq!(
            file_uri_to_path("file:///tmp/a"),
            Some(PathBuf::from("/tmp/a"))
        );
        assert_eq!(file_uri_to_path("https://x"), None);
    }
}

pub async fn send(conn: &Connection, path: &Path) -> Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    let name = path
        .file_name()
        .context("no file name")?
        .to_string_lossy()
        .into_owned();
    let header = serde_json::to_vec(&FileHeader { name, size, dir: None })?;

    let mut stream = conn.open_uni().await?.await?;
    let mut prefix = vec![STREAM_FILE];
    prefix.extend_from_slice(&(header.len() as u32).to_le_bytes());
    prefix.extend_from_slice(&header);
    stream.write_all(&prefix).await?;

    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n]).await?;
    }
    stream.finish().await?;
    info!("sent file {} ({size} bytes)", path.display());
    Ok(())
}
