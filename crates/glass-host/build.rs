// Windows: a version resource on the programs (product name, version, publisher), so
// Explorer, SmartScreen and code signing see what they are, and the Glass icon as icon 1:
// Explorer shows it, and the app's window (GPUI loads icon 1) shows it in the title bar,
// the taskbar and Alt+Tab. Nothing to do elsewhere.
fn main() {
    println!("cargo:rerun-if-changed=assets/glass.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/glass.ico");
    res.set("ProductName", "Glass")
        .set("FileDescription", "Glass remote desktop")
        .set("CompanyName", "Glass")
        .set("LegalCopyright", "Copyright (c) 2026 Nathan Tarasiuk (MIT License)");
    if let Err(e) = res.compile() {
        panic!("Windows version resource: {e}");
    }
}
