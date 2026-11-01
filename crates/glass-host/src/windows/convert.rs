//! The Windows port's arithmetic and naming, with no Win32 calls, so its tests run on any
//! OS: an app's id from its executable.

/// An app's id, what grants and protected apps name it by: the executable's file name
/// without `.exe`, lowercase ("windowsterminal"). Stable, unlike its localized description.
pub fn app_id(exe_path: &str) -> Option<String> {
    let file = exe_path.rsplit(['\\', '/']).next()?;
    let stem = match file.rsplit_once('.') {
        Some((stem, ext)) if ext.eq_ignore_ascii_case("exe") => stem,
        _ => file,
    };
    (!stem.is_empty()).then(|| stem.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_the_lowercase_exe_stem() {
        assert_eq!(app_id(r"C:\Program Files\WindowsApps\Microsoft.WindowsTerminal\WindowsTerminal.exe").as_deref(), Some("windowsterminal"));
        assert_eq!(app_id(r"C:\Windows\System32\cmd.EXE").as_deref(), Some("cmd"));
        assert_eq!(app_id(r"C:\tools\my.app.exe").as_deref(), Some("my.app"));
        assert_eq!(app_id(r"C:\tools\noext").as_deref(), Some("noext"));
        assert_eq!(app_id(r"C:\dir\"), None);
    }
}
