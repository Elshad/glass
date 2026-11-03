// Windows: a version resource on the programs (product name, version, publisher), so
// Explorer, SmartScreen and code signing see what they are. Nothing to do elsewhere.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "Glass")
        .set("FileDescription", "Glass remote desktop")
        .set("CompanyName", "Glass")
        .set("LegalCopyright", "Copyright (c) 2026 Nathan Tarasiuk (MIT License)");
    if let Err(e) = res.compile() {
        panic!("Windows version resource: {e}");
    }
}
