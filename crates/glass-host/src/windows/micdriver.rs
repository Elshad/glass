//! Glass Microphone's driver on Windows (native/glass-mic-windows): a root-enumerated
//! device, installed and removed here the way `devcon install` / `devcon remove` would.
//! Both need administrator rights; `glass mic install` runs them elevated (one UAC prompt).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIGCF_ALLCLASSES, DIGCF_PRESENT, DiUninstallDevice, HDEVINFO, INSTALLFLAG_FORCE,
    SP_DEVINFO_DATA, SPDRP_HARDWAREID, SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiGetINFClassW, SetupDiSetDeviceRegistryPropertyW, UpdateDriverForPlugAndPlayDevicesW,
};
use windows::Win32::Foundation::HWND;
use windows::core::{BOOL, GUID, HSTRING, PCWSTR, w};

const HARDWARE_ID: &str = r"ROOT\GlassMicrophone";
/// What the elevated copy of glass.exe is started with: `<arg> install <inf>` / `<arg> remove`.
pub const ARG: &str = "--glass-mic-driver";

/// The driver package shipped with Glass (`share\glass\drivers\glassmic`), or the one
/// `GLASS_MIC_DRIVER` points at (a folder with glassmic.inf).
pub fn package() -> Option<PathBuf> {
    let dir = match std::env::var_os("GLASS_MIC_DRIVER") {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_exe().ok()?.parent()?.parent()?.join(r"share\glass\drivers\glassmic"),
    };
    let inf = dir.join("glassmic.inf");
    inf.exists().then_some(inf)
}

/// Whether the Glass Microphone device is there.
pub fn installed() -> bool {
    devices(|_, _| true).unwrap_or(0) > 0
}

/// Installs the driver from `inf`, adding the device if it isn't there yet. Needs
/// administrator rights. True if Windows wants a restart to finish.
pub fn install(inf: &Path) -> Result<bool> {
    let inf = std::fs::canonicalize(inf).with_context(|| format!("no driver at {}", inf.display()))?;
    // canonicalize gives a \\?\ path, which the installer doesn't take.
    let inf = PathBuf::from(inf.to_string_lossy().trim_start_matches(r"\\?\"));
    let inf_w = HSTRING::from(inf.as_os_str());
    if !installed() {
        // SAFETY: buffers are sized as passed; the device info list is destroyed below.
        unsafe {
            let mut class = GUID::zeroed();
            let mut class_name = [0u16; 64];
            SetupDiGetINFClassW(&inf_w, &mut class, &mut class_name, None).context("read the driver's INF")?;
            let set = SetupDiCreateDeviceInfoList(Some(&class), None).context("SetupDiCreateDeviceInfoList")?;
            let result = (|| -> Result<()> {
                let mut device = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
                SetupDiCreateDeviceInfoW(set, PCWSTR(class_name.as_ptr()), &class, PCWSTR::null(), None, DICD_GENERATE_ID, Some(&mut device))
                    .context("create the device")?;
                let ids: Vec<u8> = HARDWARE_ID.encode_utf16().chain([0, 0]).flat_map(u16::to_le_bytes).collect();
                SetupDiSetDeviceRegistryPropertyW(set, &mut device, SPDRP_HARDWAREID, Some(&ids)).context("set its hardware ID")?;
                SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&device)).context("register the device")?;
                Ok(())
            })();
            let _ = SetupDiDestroyDeviceInfoList(set);
            result?;
        }
    }
    let mut reboot = BOOL(0);
    // SAFETY: NUL-terminated strings; reboot is a local.
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(None, &HSTRING::from(HARDWARE_ID), &inf_w, INSTALLFLAG_FORCE, Some(&mut reboot))
            .context("install the driver")?;
    }
    Ok(reboot.as_bool())
}

/// Removes the Glass Microphone device(s). Needs administrator rights.
pub fn remove() -> Result<usize> {
    devices(|set, device| {
        // SAFETY: a device from the live list being enumerated.
        unsafe { DiUninstallDevice(HWND::default(), set, device, 0, None).is_ok() }
    })
}

/// Calls `f` for each present device with Glass Microphone's hardware ID; how many said yes.
fn devices(mut f: impl FnMut(HDEVINFO, &SP_DEVINFO_DATA) -> bool) -> Result<usize> {
    // SAFETY: the list is destroyed before returning; buffers are sized as passed.
    unsafe {
        let set = SetupDiGetClassDevsW(None, w!("ROOT"), None, DIGCF_ALLCLASSES | DIGCF_PRESENT).context("list devices")?;
        let mut count = 0;
        let mut i = 0;
        loop {
            let mut device = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInfo(set, i, &mut device).is_err() {
                break;
            }
            i += 1;
            let mut buf = [0u8; 512];
            if SetupDiGetDeviceRegistryPropertyW(set, &device, SPDRP_HARDWAREID, None, Some(&mut buf), None).is_err() {
                continue;
            }
            let ids: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let matches = String::from_utf16_lossy(&ids).split('\0').any(|id| id.eq_ignore_ascii_case(HARDWARE_ID));
            if matches && f(set, &device) {
                count += 1;
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        Ok(count)
    }
}

/// The elevated side: `--glass-mic-driver install <inf>` or `--glass-mic-driver remove`.
pub fn cli(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("install") => {
            let inf = args.get(1).context("which INF?")?;
            if install(Path::new(inf))? {
                println!("Installed Glass Microphone; restart the computer to finish.");
            } else {
                println!("Installed Glass Microphone.");
            }
        }
        Some("remove") => println!("Removed {} Glass Microphone device(s).", remove()?),
        _ => anyhow::bail!("usage: {ARG} install <inf> | remove"),
    }
    Ok(())
}

/// Runs `cli` as administrator (one UAC prompt), unless already elevated.
pub fn elevated(args: &[&str]) -> Result<()> {
    if super::service::is_elevated() {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        return cli(&args);
    }
    let exe = std::env::current_exe()?;
    let ps = super::ps_string;
    // Each quoted: Start-Process joins them with spaces, and a path can have one.
    let list = std::iter::once(ARG).chain(args.iter().copied()).map(|a| ps(&format!("\"{a}\""))).collect::<Vec<_>>().join(",");
    let script = format!(
        "$ErrorActionPreference = 'Stop'; $p = Start-Process -FilePath {} -ArgumentList {list} -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
        ps(&exe.display().to_string())
    );
    let ok = super::powershell(&script).status().is_ok_and(|s| s.success());
    anyhow::ensure!(ok, "administrator approval was declined, or the driver didn't install");
    Ok(())
}
