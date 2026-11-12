//! Glass in the Start menu: a shortcut to `glassw.exe app` (glassw has no console window,
//! so none flashes up), with the Glass icon. There's one binary, and updating it updates
//! the app. Installed for everyone, the MSI adds the all-users shortcut instead.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn path() -> PathBuf {
    crate::platform::remove::start_menu_link()
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
    std::fs::write(&icon, include_bytes!("../../assets/glass.ico"))?;
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
    // And to Settings › Apps, so it can be uninstalled from there.
    crate::platform::remove::register()?;
    Ok(link)
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
    /// The Glass icon has the sizes Windows asks for, small (16) to large (256).
    #[test]
    fn the_icon_has_every_size() {
        let ico = include_bytes!("../../assets/glass.ico");
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        // Width 0 means 256.
        let sizes: Vec<u32> = (0..count).map(|i| match ico[6 + 16 * i] { 0 => 256, w => w as u32 }).collect();
        for size in [16, 24, 32, 48, 256] {
            assert!(sizes.contains(&size), "no {size}px icon in {sizes:?}");
        }
    }
}
