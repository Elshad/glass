//! Notifications as Windows toasts in Glass's own name: an app id registered under
//! HKCU\Software\Classes\AppUserModelId (name and icon) is all an unpackaged program needs.
//! Shown from a thread of the host itself, so they appear at once.

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
use windows::core::HSTRING;

const APP_ID: &str = "Glassdesk.Glass";

pub fn show(title: &str, body: &str) {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape(title),
        escape(body)
    );
    std::thread::spawn(move || {
        let shown = super::system::with_com(|| {
            register();
            Some(toast(&xml))
        });
        if let Some(Err(e)) = shown {
            tracing::warn!("couldn't show a notification: {e}");
        }
    });
}

fn toast(xml: &str) -> windows::core::Result<()> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)
}

/// The app id's name and icon, so toasts say "Glass" with its icon.
fn register() {
    let icon = super::local_app_data().join("Glass").join("icon.png");
    if !icon.exists() {
        let _ = std::fs::create_dir_all(icon.parent().unwrap());
        let _ = std::fs::write(&icon, include_bytes!("../../assets/icon.png"));
    }
    let key = HSTRING::from(format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
    for (name, value) in [("DisplayName", "Glass".to_owned()), ("IconUri", icon.display().to_string())] {
        let v: Vec<u16> = value.encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated UTF-16 string and its size in bytes; creates the key.
        let _ = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, &key, &HSTRING::from(name), REG_SZ.0, Some(v.as_ptr().cast()), (v.len() * 2) as u32) };
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
