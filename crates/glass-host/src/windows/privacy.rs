//! Privacy screen on Windows: not yet (it needs an overlay the duplication skips, or a
//! virtual display driver).

pub const SUPPORTED: bool = false;

pub fn set(on: bool) -> anyhow::Result<()> {
    let _ = on;
    anyhow::bail!("privacy mode isn't available on Windows yet")
}
