//! The Windows port's arithmetic and naming, with no Win32 calls, so its tests run on any
//! OS: an app's id from its executable, and UI Automation control types as the short
//! roles snapshots show.

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

/// UIA_*ControlTypeId values (UIAutomationClient.h).
pub mod control {
    pub const BUTTON: i32 = 50000;
    pub const CALENDAR: i32 = 50001;
    pub const CHECK_BOX: i32 = 50002;
    pub const COMBO_BOX: i32 = 50003;
    pub const EDIT: i32 = 50004;
    pub const HYPERLINK: i32 = 50005;
    pub const IMAGE: i32 = 50006;
    pub const LIST_ITEM: i32 = 50007;
    pub const LIST: i32 = 50008;
    pub const MENU: i32 = 50009;
    pub const MENU_BAR: i32 = 50010;
    pub const MENU_ITEM: i32 = 50011;
    pub const PROGRESS_BAR: i32 = 50012;
    pub const RADIO_BUTTON: i32 = 50013;
    pub const SCROLL_BAR: i32 = 50014;
    pub const SLIDER: i32 = 50015;
    pub const SPINNER: i32 = 50016;
    pub const STATUS_BAR: i32 = 50017;
    pub const TAB: i32 = 50018;
    pub const TAB_ITEM: i32 = 50019;
    pub const TEXT: i32 = 50020;
    pub const TOOL_BAR: i32 = 50021;
    pub const TOOL_TIP: i32 = 50022;
    pub const TREE: i32 = 50023;
    pub const TREE_ITEM: i32 = 50024;
    pub const CUSTOM: i32 = 50025;
    pub const GROUP: i32 = 50026;
    pub const THUMB: i32 = 50027;
    pub const DATA_GRID: i32 = 50028;
    pub const DATA_ITEM: i32 = 50029;
    pub const DOCUMENT: i32 = 50030;
    pub const SPLIT_BUTTON: i32 = 50031;
    pub const WINDOW: i32 = 50032;
    pub const PANE: i32 = 50033;
    pub const HEADER: i32 = 50034;
    pub const HEADER_ITEM: i32 = 50035;
    pub const TABLE: i32 = 50036;
    pub const TITLE_BAR: i32 = 50037;
    pub const SEPARATOR: i32 = 50038;
    pub const SEMANTIC_ZOOM: i32 = 50039;
    pub const APP_BAR: i32 = 50040;
}

/// The short role agents see, in the same words as on macOS where there's a match:
/// "button", "textfield", "passwordfield", "checkbox", "link", "text".
pub fn short_role(control_type: i32, password: bool) -> &'static str {
    use control::*;
    match control_type {
        EDIT if password => "passwordfield",
        BUTTON => "button",
        CALENDAR => "calendar",
        CHECK_BOX => "checkbox",
        COMBO_BOX => "combobox",
        EDIT => "textfield",
        HYPERLINK => "link",
        IMAGE => "image",
        LIST_ITEM => "listitem",
        LIST => "list",
        MENU => "menu",
        MENU_BAR => "menubar",
        MENU_ITEM => "menuitem",
        PROGRESS_BAR => "progressbar",
        RADIO_BUTTON => "radio",
        SCROLL_BAR => "scrollbar",
        SLIDER => "slider",
        SPINNER => "spinner",
        STATUS_BAR => "statusbar",
        TAB => "tabgroup",
        TAB_ITEM => "tab",
        TEXT => "text",
        TOOL_BAR | APP_BAR => "toolbar",
        TOOL_TIP => "tooltip",
        TREE => "tree",
        TREE_ITEM => "treeitem",
        GROUP => "group",
        THUMB => "thumb",
        DATA_GRID | TABLE => "table",
        DATA_ITEM => "row",
        DOCUMENT => "document",
        SPLIT_BUTTON => "splitbutton",
        WINDOW => "window",
        PANE | SEMANTIC_ZOOM => "pane",
        HEADER => "header",
        HEADER_ITEM => "headeritem",
        TITLE_BAR => "titlebar",
        SEPARATOR => "separator",
        _ => "custom",
    }
}

/// Control types an agent can act on whatever patterns they offer.
pub fn interactive(control_type: i32) -> bool {
    use control::*;
    matches!(
        control_type,
        BUTTON | CHECK_BOX | COMBO_BOX | HYPERLINK | LIST_ITEM | MENU_ITEM | RADIO_BUTTON | SLIDER | SPINNER
            | TAB_ITEM | TREE_ITEM | SPLIT_BUTTON | DATA_ITEM | HEADER_ITEM
    )
}

/// Control types whose value is text someone can replace (when it isn't read-only).
pub fn editable(control_type: i32) -> bool {
    use control::*;
    matches!(control_type, EDIT | COMBO_BOX | DOCUMENT)
}

/// Unnamed containers that do nothing: scaffolding, not counted against the depth limit.
pub fn scaffolding(control_type: i32) -> bool {
    use control::*;
    matches!(control_type, GROUP | PANE | CUSTOM)
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

    #[test]
    fn roles_read_like_the_mac_ones() {
        assert_eq!(short_role(control::EDIT, false), "textfield");
        assert_eq!(short_role(control::EDIT, true), "passwordfield");
        assert_eq!(short_role(control::RADIO_BUTTON, false), "radio");
        assert_eq!(short_role(control::TAB_ITEM, false), "tab");
        assert_eq!(short_role(control::TEXT, false), "text");
        assert_eq!(short_role(1234, false), "custom");
        assert!(interactive(control::BUTTON) && !interactive(control::TEXT));
        assert!(editable(control::EDIT) && !editable(control::BUTTON));
    }
}
