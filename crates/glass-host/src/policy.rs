//! Policies: settings an administrator sets for the whole computer, through Group Policy or
//! an MDM like Intune. They override the owner's settings, and Glass shows them as managed
//! instead of letting anyone change them. On Windows they're registry values under
//! HKLM\SOFTWARE\Policies\Glass, or HKCU\SOFTWARE\Policies\Glass for one user; the
//! machine's win. Other systems have none yet: a macOS configuration profile (managed
//! preferences) would be one more source for `load`, feeding the same table.
//!
//! packaging/windows/policies/glass.admx describes the same values for Group Policy.

// Only Windows reads policies yet; elsewhere the table is always empty.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::BTreeMap;

use anyhow::Result;

/// A value as an administrator stored it: a number (REG_DWORD) or text (REG_SZ).
#[derive(Debug, Clone, PartialEq)]
pub enum Raw {
    Number(u32),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// On or off: a DWORD (0 off, anything else on), or the text on/off/true/false/1/0.
    Switch,
    /// An https:// address, checked like `glass config relay`.
    Url,
}

/// One policy: its key (as in `glass config`), its registry value, and what it decides.
pub struct Def {
    pub key: &'static str,
    pub name: &'static str,
    kind: Kind,
    /// Kept in settings.json (so it overrides the stored setting); the rest are checked
    /// where the feature is used.
    pub setting: bool,
    pub what: &'static str,
}

const fn switch(key: &'static str, name: &'static str, setting: bool, what: &'static str) -> Def {
    Def { key, name, kind: Kind::Switch, setting, what }
}

pub const DEFS: &[Def] = &[
    switch("ask-guests", "AskGuests", true, "ask before a guest (link) connects"),
    switch("ask-everyone", "AskEveryone", true, "ask before anyone connects"),
    switch("privacy", "PrivacyScreen", true, "blank this screen and block local input while viewed"),
    switch("remote", "ReachFromAnywhere", false, "reach this computer from anywhere (glass remote)"),
    switch("agents", "Agents", false, "AI agents on other devices may ask to use this computer"),
    Def { key: "relay", name: "Relay", kind: Kind::Url, setting: true, what: "self-hosted remote-access server" },
    switch("links", "GuestLinks", false, "guest links (glass link)"),
    switch("support", "QuickSupport", false, "quick support (glass support)"),
    switch("files", "FileTransfer", false, "sending and getting files"),
    switch("clipboard", "Clipboard", false, "copy and paste with viewers"),
    switch("terminal", "Terminal", false, "the owner's terminal in the viewer"),
    switch("updates", "Updates", false, "checking for and installing updates"),
];

/// What `glass config` and errors say about a managed setting.
pub const MANAGED: &str = "managed by your organization";

/// The policies in force, by key, in `glass config`'s text form ("on", "off", a URL).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Policy(BTreeMap<&'static str, String>);

impl Policy {
    /// Reads each policy from the first source that sets it validly (the machine's first).
    /// A value that doesn't parse is skipped, as if unset, and logged.
    pub fn merge(sources: &[&dyn Fn(&str) -> Option<Raw>]) -> Self {
        let mut out = BTreeMap::new();
        for def in DEFS {
            for source in sources {
                let Some(raw) = source(def.name) else { continue };
                match parse(def.kind, &raw) {
                    Some(v) => {
                        out.insert(def.key, v);
                        break;
                    }
                    None => tracing::warn!("policy {}: ignoring {raw:?}", def.name),
                }
            }
        }
        Self(out)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn locked(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// An on/off policy's value, if one is set.
    pub fn flag(&self, key: &str) -> Option<bool> {
        self.get(key).map(|v| v == "on")
    }

    /// Whether a feature an administrator can turn off is allowed (not set: yes).
    pub fn allows(&self, key: &str) -> bool {
        self.flag(key) != Some(false)
    }

    /// Every policy in force, with what it decides.
    pub fn iter(&self) -> impl Iterator<Item = (&'static Def, &str)> {
        DEFS.iter().filter_map(|d| Some((d, self.get(d.key)?)))
    }

    /// Those kept in settings.json.
    pub fn settings(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.iter().filter(|(d, _)| d.setting).map(|(d, v)| (d.key, v))
    }

    /// Refuses to change `key` to anything but the value it's locked to.
    pub fn check(&self, key: &str, value: &str) -> Result<()> {
        let (Some(locked), Some(def)) = (self.get(key), DEFS.iter().find(|d| d.key == key)) else { return Ok(()) };
        let same = parse(def.kind, &Raw::Text(value.into())).is_some_and(|v| v == locked);
        anyhow::ensure!(same, "{key} is {MANAGED} (it's {locked}); ask your IT administrator");
        Ok(())
    }

    /// Refuses a feature an administrator turned off.
    pub fn require(&self, key: &str) -> Result<()> {
        let what = DEFS.iter().find(|d| d.key == key).map_or(key, |d| d.what);
        anyhow::ensure!(self.allows(key), "{what}: turned off by your organization; ask your IT administrator");
        Ok(())
    }
}

fn parse(kind: Kind, raw: &Raw) -> Option<String> {
    match (kind, raw) {
        (Kind::Switch, Raw::Number(n)) => Some(if *n != 0 { "on" } else { "off" }.into()),
        (Kind::Switch, Raw::Text(t)) => match t.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Some("on".into()),
            "off" | "false" | "no" | "0" => Some("off".into()),
            _ => None,
        },
        // Group Policy writes an empty string for an enabled policy left blank: not set.
        (Kind::Url, Raw::Text(t)) if t.trim().is_empty() => None,
        (Kind::Url, Raw::Text(t)) => crate::settings::validate_relay(t).ok(),
        (Kind::Url, Raw::Number(_)) => None,
    }
}

/// The policies in force now. Cheap enough to read wherever they matter, like settings.
pub fn load() -> Policy {
    #[cfg(windows)]
    return crate::platform::policy::load();
    #[cfg(not(windows))]
    Policy::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from<'a>(values: &'a [(&'a str, Raw)]) -> impl Fn(&str) -> Option<Raw> + 'a {
        move |name| values.iter().find(|(n, _)| *n == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn the_machine_wins_and_bad_values_are_skipped() {
        let machine = [("AskGuests", Raw::Number(1)), ("Relay", Raw::Text("ftp://nope".into())), ("Terminal", Raw::Number(0))];
        let user = [
            ("AskGuests", Raw::Number(0)),
            ("Relay", Raw::Text("https://glass.example.com/".into())),
            ("FileTransfer", Raw::Text("off".into())),
            ("Clipboard", Raw::Text("maybe".into())),
            ("Updates", Raw::Text("".into())),
        ];
        let p = Policy::merge(&[&from(&machine), &from(&user)]);
        assert_eq!(p.flag("ask-guests"), Some(true));
        assert_eq!(p.get("relay"), Some("https://glass.example.com"), "the machine's is invalid, so the user's");
        assert!(!p.allows("terminal") && !p.allows("files"));
        assert!(p.allows("clipboard") && !p.locked("clipboard"), "unparseable: not set");
        assert!(p.allows("updates") && !p.locked("updates"));
        assert!(p.allows("remote") && p.flag("remote").is_none());
        assert_eq!(p.settings().collect::<Vec<_>>(), vec![("ask-guests", "on"), ("relay", "https://glass.example.com")]);
    }

    #[test]
    fn locked_values_can_only_be_set_to_themselves() {
        let machine = [("PrivacyScreen", Raw::Number(1)), ("Relay", Raw::Text("https://r.example.com".into()))];
        let p = Policy::merge(&[&from(&machine)]);
        assert!(p.check("privacy", "on").is_ok());
        assert!(p.check("privacy", "true").is_ok());
        assert!(p.check("privacy", "off").unwrap_err().to_string().contains(MANAGED));
        assert!(p.check("relay", "https://r.example.com/").is_ok());
        assert!(p.check("relay", "").is_err());
        assert!(p.check("ask-guests", "off").is_ok(), "not managed");
        assert!(p.require("files").is_ok());
        let off = Policy::merge(&[&from(&[("QuickSupport", Raw::Number(0))])]);
        assert!(off.require("support").unwrap_err().to_string().contains("quick support"));
    }

    #[test]
    fn the_group_policy_template_has_every_policy() {
        let admx = include_str!("../../../packaging/windows/policies/glass.admx");
        for d in DEFS {
            assert!(admx.contains(&format!("valueName=\"{}\"", d.name)), "{} isn't in glass.admx", d.name);
        }
        assert_eq!(admx.matches("<policy ").count(), DEFS.len(), "glass.admx has a policy Glass doesn't read");
    }

    #[test]
    fn nothing_set_is_empty() {
        let p = Policy::merge(&[&|_: &str| None]);
        assert!(p.is_empty() && p.iter().next().is_none());
        assert!(DEFS.iter().all(|d| p.allows(d.key)));
    }
}
