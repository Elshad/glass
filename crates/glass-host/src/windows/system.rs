//! What the shared code asks of the system on Windows: the fleet report's details, local
//! time, the computer's name and the Recycle Bin. Win32 calls, so no console tools.

use std::path::Path;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::NetworkManagement::WindowsFirewall::{INetFwPolicy2, NET_FW_PROFILE_TYPE2, NetFwPolicy2};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::{
    ComputerNamePhysicalDnsHostname, GetComputerNameExW, GetPhysicallyInstalledSystemMemory, GetTickCount64, GlobalMemoryStatusEx,
    MEMORYSTATUSEX,
};
use windows::Win32::System::Time::{
    DYNAMIC_TIME_ZONE_INFORMATION, FileTimeToSystemTime, GetDynamicTimeZoneInformation, SystemTimeToTzSpecificLocalTimeEx,
};
use windows::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// Runs `f` with COM ready on this thread (the host's blocking threads never set it up).
pub fn with_com<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    // SAFETY: paired with CoUninitialize below when it succeeds. RPC_E_CHANGED_MODE means
    // the thread already has COM (single-threaded), which works as well.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let out = f();
    if hr.is_ok() {
        // SAFETY: balances the successful CoInitializeEx above, on the same thread.
        unsafe { CoUninitialize() };
    }
    out
}

/// "Surface Laptop 5", from what the firmware says.
pub fn model() -> String {
    const KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
    let get = |v: &str| super::registry_string(KEY, v).unwrap_or_default();
    crate::fleet::windows_model(&get("SystemManufacturer"), &get("SystemProductName"), &get("SystemVersion"))
}

/// "Intel(R) Core(TM) i7-1185G7 @ 3.00GHz" (Intel pads it with spaces).
pub fn cpu_name() -> String {
    super::registry_string(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString")
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// Installed memory in whole GB.
pub fn ram_gb() -> u32 {
    let mut kb = 0u64;
    // SAFETY: kb is a valid out pointer.
    if unsafe { GetPhysicallyInstalledSystemMemory(&mut kb) }.is_ok() && kb > 0 {
        return (kb / 1_048_576) as u32;
    }
    // What the OS can use, a little under what's installed: round to the nearest GB.
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: m is sized as the call requires.
    if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() { ((m.ullTotalPhys + (1 << 29)) >> 30) as u32 } else { 0 }
}

/// (total GB, free GB) of the drive Windows is on.
pub fn system_disk_gb() -> (u32, u32) {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: both out pointers are valid for the call.
    if unsafe { GetDiskFreeSpaceExW(&HSTRING::from(format!("{drive}\\")), Some(&mut free), Some(&mut total), None) }.is_err() {
        return (0, 0);
    }
    ((total >> 30) as u32, (free >> 30) as u32)
}

pub fn uptime_h() -> u32 {
    // SAFETY: no preconditions.
    (unsafe { GetTickCount64() } / 3_600_000) as u32
}

/// Percent, when there's a battery.
pub fn battery() -> Option<u8> {
    let mut s = SYSTEM_POWER_STATUS::default();
    // SAFETY: s is a valid out pointer.
    unsafe { GetSystemPowerStatus(&mut s) }.ok()?;
    crate::fleet::windows_battery(s.BatteryFlag, s.BatteryLifePercent)
}

/// BitLocker on the system drive. The volume's shell property needs no administrator
/// (manage-bde and Get-BitLockerVolume do).
pub fn bitlocker() -> Option<bool> {
    let out = super::powershell(
        "(New-Object -ComObject Shell.Application).NameSpace($env:SystemDrive).Self.ExtendedProperty('System.Volume.BitLockerProtection')",
    )
    .output()
    .ok()?;
    crate::fleet::parse_bitlocker(&String::from_utf8_lossy(&out.stdout))
}

/// Windows Defender Firewall is on for every network profile in use (any user can read it).
pub fn firewall() -> Option<bool> {
    with_com(|| {
        // SAFETY: COM is initialised on this thread; the policy object is released on drop.
        unsafe {
            let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER).ok()?;
            let current = policy.CurrentProfileTypes().ok()?;
            let mut any = false;
            for bit in [1, 2, 4] {
                if current & bit != 0 {
                    any = true;
                    if !policy.get_FirewallEnabled(NET_FW_PROFILE_TYPE2(bit)).ok()?.as_bool() {
                        return Some(false);
                    }
                }
            }
            any.then_some(true)
        }
    })
}

/// Local "YYYY-MM-DD HH:MM" for a Unix time, by the current time zone's rules.
pub fn local_time(unix: u64) -> Option<String> {
    // FILETIME counts 100 ns from 1601.
    let ticks = unix.checked_mul(10_000_000)?.checked_add(116_444_736_000_000_000)?;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    let mut tz = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: every pointer is to a local of the type the call expects.
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        GetDynamicTimeZoneInformation(&mut tz);
        SystemTimeToTzSpecificLocalTimeEx(Some(&tz), &utc, &mut local).ok()?;
    }
    Some(format!("{:04}-{:02}-{:02} {:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute))
}

/// The computer's name as its owner typed it ("Mums-Laptop"), falling back to the
/// upper-case NetBIOS one.
pub fn computer_name() -> Option<String> {
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: buf holds len UTF-16 units; len is updated to the name's length.
    let ok = unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, Some(PWSTR(buf.as_mut_ptr())), &mut len) }.is_ok();
    let name = if ok { String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]) } else { String::new() };
    Some(name).filter(|n| !n.is_empty()).or_else(|| std::env::var("COMPUTERNAME").ok().filter(|n| !n.is_empty()))
}

/// Sends a file or folder to the Recycle Bin. Network drives have none, so they're refused
/// (Windows would otherwise ask on the computer's screen whether to delete it outright, and
/// the request would wait for someone there); one too big for the bin still asks.
pub fn recycle(path: &Path) -> Result<()> {
    let shell = crate::browse::shell_path(&path.to_string_lossy());
    if is_network(&shell) {
        bail!("files on a network drive can't go to the Recycle Bin");
    }
    // The shell takes neither \\?\ paths nor a single NUL: the list ends with two.
    let from: Vec<u16> = shell.encode_utf16().chain([0, 0]).collect();
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING).0 as u16,
        ..Default::default()
    };
    // SAFETY: pFrom is a double-NUL-terminated list that outlives the call.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 || op.fAnyOperationsAborted.as_bool() {
        bail!("couldn't move it to the Recycle Bin (error {code:#x})");
    }
    Ok(())
}

/// The shell's own thumbnail of a file (what Explorer shows for pictures, PDFs and videos),
/// fitting in `size`×`size`: width, height and BGRA rows top to bottom, alpha premultiplied
/// as GDI keeps it. `None` for a file without one: no icon instead, the viewer has its own.
pub fn thumbnail(path: &Path, size: u32) -> Option<(usize, usize, Vec<u8>)> {
    // SAFETY: paired with CoUninitialize below when it succeeds. Thumbnail handlers are
    // mostly apartment-threaded, so a single-threaded apartment saves marshalling to one; a
    // thread that already has COM (RPC_E_CHANGED_MODE) works as well.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let out = shell_thumbnail(path, size);
    if hr.is_ok() {
        // SAFETY: balances the successful CoInitializeEx above, on the same thread.
        unsafe { CoUninitialize() };
    }
    out
}

fn shell_thumbnail(path: &Path, size: u32) -> Option<(usize, usize, Vec<u8>)> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW,
    };
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY};

    let shell = HSTRING::from(crate::browse::shell_path(&path.to_string_lossy()));
    let side = size as i32;
    // SAFETY: COM is initialised on this thread (thumbnail() above). The bitmap the factory
    // returns is ours: read into a buffer sized from its own header, then deleted. The
    // memory DC is only a context for GetDIBits (the bitmap is never selected into it).
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&shell, None).ok()?;
        let bmp = factory.GetImage(SIZE { cx: side, cy: side }, SIIGBF_THUMBNAILONLY).ok()?;
        let mut head = BITMAP::default();
        let got = GetObjectW(bmp.into(), std::mem::size_of::<BITMAP>() as i32, Some((&mut head as *mut BITMAP).cast()));
        let (w, h) = (head.bmWidth, head.bmHeight.abs());
        let mut pixels = vec![0u8; w.max(0) as usize * h as usize * 4];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative: rows top to bottom.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = if got > 0 && w > 0 && h > 0 {
            let dc = CreateCompatibleDC(None);
            let n = GetDIBits(dc, bmp, 0, h as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
            let _ = DeleteDC(dc);
            n
        } else {
            0
        };
        let _ = DeleteObject(bmp.into());
        (lines == h && h > 0).then_some((w as usize, h as usize, pixels))
    }
}

/// A UNC path, or one on a mapped network drive.
fn is_network(path: &str) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    if path.starts_with(r"\\") {
        return true;
    }
    let Some(root) = path.get(..3).filter(|r| r.ends_with(":\\")) else { return false };
    let root: Vec<u16> = root.encode_utf16().chain([0]).collect();
    const DRIVE_REMOTE: u32 = 4;
    // SAFETY: a NUL-terminated drive root.
    unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) == DRIVE_REMOTE }
}
