//! Policies from the registry: HKLM\SOFTWARE\Policies\Glass (Group Policy's computer
//! settings, Intune's device ones), then HKCU\SOFTWARE\Policies\Glass (per user). Both
//! keys are writable only by administrators.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_VALUE_TYPE, RRF_RT_REG_DWORD,
    RRF_RT_REG_SZ, RegCloseKey, RegGetValueW, RegOpenKeyExW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::policy::{Policy, Raw};

const KEY: &str = r"SOFTWARE\Policies\Glass";

pub fn load() -> Policy {
    // Usually neither key exists, and that's two quick calls.
    let machine = Key::open(HKEY_LOCAL_MACHINE);
    let user = Key::open(HKEY_CURRENT_USER);
    Policy::merge(&[&|n: &str| machine.as_ref()?.value(n), &|n: &str| user.as_ref()?.value(n)])
}

struct Key(HKEY);

impl Key {
    fn open(root: HKEY) -> Option<Self> {
        let mut key = HKEY::default();
        // SAFETY: `key` receives the opened key, closed on drop.
        let err = unsafe { RegOpenKeyExW(root, &HSTRING::from(KEY), None, KEY_READ, &mut key) };
        (err == ERROR_SUCCESS).then_some(Self(key))
    }

    /// A DWORD or string value.
    fn value(&self, name: &str) -> Option<Raw> {
        let mut kind = REG_VALUE_TYPE::default();
        let mut buf = [0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: buf and len describe a writable buffer of len bytes; kind is a local.
        let err = unsafe {
            RegGetValueW(
                self.0,
                PCWSTR::null(),
                &HSTRING::from(name),
                RRF_RT_REG_DWORD | RRF_RT_REG_SZ,
                Some(&mut kind),
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        if err != ERROR_SUCCESS {
            return None;
        }
        if kind == REG_DWORD {
            return Some(Raw::Number(buf[0] as u32 | (buf[1] as u32) << 16));
        }
        let chars = (len as usize / 2).saturating_sub(1).min(buf.len());
        Some(Raw::Text(String::from_utf16_lossy(&buf[..chars])))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegOpenKeyExW and is closed once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}
