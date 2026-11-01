//! Glass in the Start menu: a shortcut to `glassw.exe app` (glassw has no console window,
//! so none flashes up), with the Glass icon. There's one binary, and updating it updates
//! the app. Installed for everyone, the MSI adds the all-users shortcut instead.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join("AppData").join("Roaming"))
        .join(r"Microsoft\Windows\Start Menu\Programs\Glass.lnk")
}

/// The windowless launcher (the all-users one, if Glass is installed for everyone).
fn launcher() -> Result<PathBuf> {
    crate::agent::launcher()
}

/// Writes (or refreshes) the shortcut and its icon.
pub fn install() -> Result<PathBuf> {
    if crate::agent::machine_dir().is_some() {
        // The MSI's shortcut is there for everyone: a per-user one would be a second Glass.
        let _ = std::fs::remove_file(path());
        let all = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        return Ok(all.join(r"Microsoft\Windows\Start Menu\Programs\Glass.lnk"));
    }
    let launcher = launcher()?;
    let icon = crate::platform::local_app_data().join("Glass").join("Glass.ico");
    std::fs::create_dir_all(icon.parent().unwrap())?;
    std::fs::write(&icon, ico(include_bytes!("../../assets/icon.png")))?;
    let link = path();
    let ps = crate::platform::ps_string;
    let script = format!(
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({link}); $s.TargetPath = {target}; $s.Arguments = 'app'; \
         $s.WorkingDirectory = {dir}; $s.IconLocation = {icon}; $s.Description = 'Glass: your computer, in any browser'; $s.Save()",
        link = ps(&link.display().to_string()),
        target = ps(&launcher.display().to_string()),
        dir = ps(&launcher.parent().unwrap().display().to_string()),
        icon = ps(&icon.display().to_string()),
    );
    let status = crate::platform::powershell(&script).status().context("powershell")?;
    anyhow::ensure!(status.success(), "couldn't add Glass to the Start menu");
    Ok(link)
}

/// An .ico holding one PNG (Windows Vista and later read PNG icons).
fn ico(png: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0, 1, 0];
    // Width and height 0 mean 256 or more; then planes, bits per pixel, size and offset.
    out.extend_from_slice(&[0, 0, 0, 0, 1, 0, 32, 0]);
    out.extend_from_slice(&(png.len() as u32).to_le_bytes());
    out.extend_from_slice(&22u32.to_le_bytes());
    out.extend_from_slice(png);
    out
}

/// Opens the app without a console window.
pub fn open() {
    match launcher() {
        Ok(l) => {
            let _ = std::process::Command::new(l).arg("app").spawn();
        }
        Err(e) => tracing::warn!("open the app: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn icons_wrap_the_png() {
        let ico = super::ico(b"\x89PNGdata");
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 1, 0]);
        assert_eq!(u32::from_le_bytes(ico[14..18].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(ico[18..22].try_into().unwrap()), 22);
        assert_eq!(&ico[22..], b"\x89PNGdata");
    }
}
