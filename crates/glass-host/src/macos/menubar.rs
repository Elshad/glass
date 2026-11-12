//! The Glass menu bar icon: a screen sharing symbol (with a count) while anyone is viewing
//! this Mac, badged while a permission is missing, and a menu listing who, with controls to
//! end it. Runs the app's main thread; the host itself runs on a background tokio runtime.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSString, ns_string};

use crate::viewers::{self, Viewer};

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GlassMenuTarget"]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(openApp:))]
        fn open_app(&self, _sender: Option<&AnyObject>) {
            crate::app::open_app();
        }

        #[unsafe(method(disconnectAll:))]
        fn disconnect_all(&self, _sender: Option<&AnyObject>) {
            viewers::disconnect_all();
        }

        #[unsafe(method(toggleRemote:))]
        fn toggle_remote(&self, _sender: Option<&AnyObject>) {
            let _ = crate::remote::set_enabled(!crate::remote::enabled());
            refresh_now();
        }

        #[unsafe(method(revokeLinks:))]
        fn revoke_links(&self, _sender: Option<&AnyObject>) {
            let file = crate::access::file_for(&crate::default_token_file());
            match crate::access::revoke_all(&file) {
                Ok(n) => tracing::info!("revoked links from the menu bar ({n} guest passes)"),
                Err(e) => tracing::warn!("revoking links: {e:#}"),
            }
        }

        #[unsafe(method(quitGlass:))]
        fn quit_glass(&self, _sender: Option<&AnyObject>) {
            // A clean exit: the login item restarts Glass only after a crash, so it stays
            // quit until the next login (or `glass setup`). Viewers see it disconnect.
            tracing::info!("quit from the menu bar");
            viewers::disconnect_all();
            std::thread::sleep(std::time::Duration::from_millis(200));
            std::process::exit(0);
        }

        #[unsafe(method(updateGlass:))]
        fn update_glass(&self, _sender: Option<&AnyObject>) {
            match crate::update::start_self_update() {
                Ok(()) => UPDATING.store(true, Ordering::Relaxed),
                Err(e) => tracing::warn!("update from the menu bar: {e:#}"),
            }
            refresh_now();
        }

        #[unsafe(method(fixScreenRecording:))]
        fn fix_screen_recording(&self, _sender: Option<&AnyObject>) {
            super::permissions::open_pane(super::permissions::SCREEN_RECORDING_PANE);
        }

        #[unsafe(method(fixAccessibility:))]
        fn fix_accessibility(&self, _sender: Option<&AnyObject>) {
            super::permissions::open_pane(super::permissions::ACCESSIBILITY_PANE);
        }

        #[unsafe(method(openInBrowser:))]
        fn open_in_browser(&self, _sender: Option<&AnyObject>) {
            if let Some(url) = link() {
                let _ = std::process::Command::new("open").arg(url).spawn();
            }
        }

        #[unsafe(method(copyLink:))]
        fn copy_link(&self, _sender: Option<&AnyObject>) {
            if let Some(url) = link() {
                super::clipboard::set_text(&url);
            }
        }

        #[unsafe(method(sendMessage:))]
        fn send_message(&self, _sender: Option<&AnyObject>) {
            if let Some(rt) = RUNTIME.get() {
                rt.spawn(async {
                    if let Some(text) =
                        crate::notify::ask_text("Glass", "Message to everyone viewing this Mac:")
                            .await
                        && let Err(e) = crate::server::host_chat(&text)
                    {
                        tracing::warn!("message from the menu bar: {e:#}");
                    }
                });
            }
        }

        #[unsafe(method(togglePrivacy:))]
        fn toggle_privacy(&self, _sender: Option<&AnyObject>) {
            if let Err(e) = crate::privacy::set_forced(!crate::privacy::active()) {
                tracing::warn!("privacy from the menu bar: {e:#}");
            }
            refresh_now();
        }

        #[unsafe(method(toggleAskGuests:))]
        fn toggle_ask_guests(&self, _sender: Option<&AnyObject>) {
            let mut s = crate::settings::load();
            s.ask_guests = !s.ask_guests;
            if let Err(e) = crate::settings::save(&s) {
                tracing::warn!("settings: {e:#}");
            }
            refresh_now();
        }

        #[unsafe(method(disconnectViewer:))]
        fn disconnect_viewer(&self, sender: Option<&AnyObject>) {
            let Some(item) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) else { return };
            viewers::disconnect(item.tag() as u64);
        }

        #[unsafe(method(openHistory:))]
        fn open_history(&self, _sender: Option<&AnyObject>) {
            // A readable copy, opened in TextEdit.
            let path = crate::config_dir().join("connections.txt");
            let mut text = String::from("Glass connection history (newest last)\n\n");
            for e in crate::connlog::recent(500) {
                text.push_str(&crate::connlog::describe(&e));
                text.push('\n');
            }
            if std::fs::write(&path, text).is_ok() {
                let _ = std::process::Command::new("open").arg("-e").arg(&path).spawn();
            }
        }

        #[unsafe(method(openLogs:))]
        fn open_logs(&self, _sender: Option<&AnyObject>) {
            let _ = std::process::Command::new("open").arg(super::log_dir()).spawn();
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

struct Ui {
    item: Retained<NSStatusItem>,
    target: Retained<MenuTarget>,
}

/// Set once a self-update has started; the service restart ends this process.
static UPDATING: AtomicBool = AtomicBool::new(false);

/// What the menu shows besides the viewers.
#[derive(Clone, PartialEq)]
struct State {
    screen_recording: bool,
    accessibility: bool,
    update: Option<String>,
    updating: bool,
}

impl State {
    fn now() -> Self {
        // Before the host writes its first status, assume nothing is missing.
        let status = crate::status::current();
        Self {
            screen_recording: status.as_ref().is_none_or(|s| s.screen_recording),
            accessibility: status.as_ref().is_none_or(|s| s.accessibility),
            update: crate::update::available(),
            updating: UPDATING.load(Ordering::Relaxed),
        }
    }

    fn missing_permission(&self) -> bool {
        !self.screen_recording || !self.accessibility
    }
}

/// Where to open the client, once the host knows.
fn link() -> Option<String> {
    crate::status::current().and_then(|s| s.url)
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// The host's runtime, for menu actions that wait (a text prompt).
static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// Shows the icon and runs the app on this (the main) thread. Never returns.
pub fn run(rt: tokio::runtime::Handle) -> ! {
    let mtm = MainThreadMarker::new().expect("the menu bar runs on the main thread");
    let _ = RUNTIME.set(rt.clone());
    let app = NSApplication::sharedApplication(mtm);
    // A menu bar item only: no Dock icon, no menu bar of its own.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let target = MenuTarget::new(mtm);
    UI.with(|ui| *ui.borrow_mut() = Some(Ui { item, target }));
    refresh(&viewers::subscribe().borrow().clone(), &State::now());
    // Viewers' drawings, shown on this Mac.
    super::overlay::start(&rt);

    // Redraw when viewers come and go, when permissions or updates change (checked every
    // few seconds), and each minute (for the "3 min" labels).
    rt.spawn(async {
        let mut rx = viewers::subscribe();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        let mut shown = State::now();
        let mut ticks = 0u32;
        loop {
            tokio::select! {
                changed = rx.changed() => if changed.is_err() { return },
                _ = tick.tick() => {
                    ticks += 1;
                    let state = State::now();
                    if state == shown && ticks % 12 != 0 {
                        continue;
                    }
                    shown = state;
                }
            }
            refresh_now();
        }
    });
    app.run();
    std::process::exit(0)
}

/// Redraws on the main thread with the current viewers.
fn refresh_now() {
    let list = viewers::subscribe().borrow().clone();
    let state = State::now();
    DispatchQueue::main().exec_async(move || refresh(&list, &state));
}

fn refresh(list: &[Viewer], state: &State) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    UI.with(|ui| {
        let ui = ui.borrow();
        let Some(ui) = ui.as_ref() else { return };
        let viewing = !list.is_empty();
        if let Some(button) = ui.item.button(mtm) {
            // Badged while a permission is missing, so it's noticed before someone connects;
            // macOS's screen sharing symbol, with a count, while anyone is viewing. Always a
            // plain template image: a contentTintColor renders status items black (FB8530353),
            // which vanishes on a dark menu bar.
            let symbol = if state.missing_permission() {
                ns_string!("display.trianglebadge.exclamationmark")
            } else if viewing {
                ns_string!("rectangle.inset.filled.and.person.filled")
            } else {
                ns_string!("display")
            };
            let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                symbol,
                Some(ns_string!("Glass")),
            );
            if let Some(image) = &image {
                image.setTemplate(true);
            }
            button.setImage(image.as_deref());
            let title = if viewing {
                format!(" {}", list.len())
            } else {
                String::new()
            };
            button.setTitle(&NSString::from_str(&title));
            let tip = if viewing {
                format!("Glass: {} viewing this Mac", list.len())
            } else if state.missing_permission() {
                "Glass: needs a permission to be granted".to_string()
            } else {
                "Glass: no one is viewing this Mac".to_string()
            };
            button.setToolTip(Some(&NSString::from_str(&tip)));
        }
        ui.item.setMenu(Some(&menu(mtm, &ui.target, list, state)));
    });
}

fn menu(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    list: &[Viewer],
    state: &State,
) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let label = |text: &str| {
        let item = item(mtm, text, None, target);
        item.setEnabled(false);
        item
    };
    menu.addItem(&label(&format!("Glass {}", crate::update::CURRENT)));
    let open_app = item(mtm, "Open Glass…", Some(sel!(openApp:)), target);
    open_app.setKeyEquivalent(ns_string!(","));
    menu.addItem(&open_app);
    if state.updating {
        menu.addItem(&label("Updating…"));
    } else if let Some(v) = &state.update {
        menu.addItem(&item(
            mtm,
            &format!("Update to {v}"),
            Some(sel!(updateGlass:)),
            target,
        ));
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    if !state.screen_recording {
        menu.addItem(&item(
            mtm,
            "Allow Screen Recording…",
            Some(sel!(fixScreenRecording:)),
            target,
        ));
    }
    if !state.accessibility {
        menu.addItem(&item(
            mtm,
            "Allow Accessibility (for control)…",
            Some(sel!(fixAccessibility:)),
            target,
        ));
    }
    if state.missing_permission() {
        menu.addItem(&NSMenuItem::separatorItem(mtm));
    }
    if list.is_empty() {
        menu.addItem(&label("No one is viewing this Mac"));
    } else {
        let heading = match list.len() {
            1 => "1 viewer".to_string(),
            n => format!("{n} viewers"),
        };
        menu.addItem(&label(&heading));
        for v in list {
            // Choosing a viewer disconnects just that one.
            let row = item(
                mtm,
                &format!("   {}", v.describe()),
                Some(sel!(disconnectViewer:)),
                target,
            );
            row.setTag(v.id as isize);
            row.setToolTip(Some(ns_string!("Disconnect this viewer")));
            menu.addItem(&row);
        }
        menu.addItem(&item(
            mtm,
            "Send a message…",
            Some(sel!(sendMessage:)),
            target,
        ));
        if crate::privacy::supported() {
            let privacy = item(
                mtm,
                "Privacy screen (blank this Mac)",
                Some(sel!(togglePrivacy:)),
                target,
            );
            privacy.setState(if crate::privacy::active() { 1 } else { 0 });
            menu.addItem(&privacy);
        }
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let has_link = link().is_some();
    let open = item(mtm, "Open in browser", Some(sel!(openInBrowser:)), target);
    open.setEnabled(has_link);
    menu.addItem(&open);
    let copy = item(mtm, "Copy link", Some(sel!(copyLink:)), target);
    copy.setEnabled(has_link);
    menu.addItem(&copy);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let disconnect = item(
        mtm,
        "Disconnect everyone",
        Some(sel!(disconnectAll:)),
        target,
    );
    disconnect.setEnabled(!list.is_empty());
    menu.addItem(&disconnect);
    let remote = item(
        mtm,
        "Remote access from anywhere",
        Some(sel!(toggleRemote:)),
        target,
    );
    remote.setState(if crate::remote::enabled() { 1 } else { 0 });
    menu.addItem(&remote);
    let ask = item(
        mtm,
        "Ask before guests connect",
        Some(sel!(toggleAskGuests:)),
        target,
    );
    ask.setState(if crate::settings::load().ask_guests { 1 } else { 0 });
    menu.addItem(&ask);
    menu.addItem(&item(
        mtm,
        "Revoke all guest links",
        Some(sel!(revokeLinks:)),
        target,
    ));
    menu.addItem(&item(
        mtm,
        "Connection history",
        Some(sel!(openHistory:)),
        target,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&item(mtm, "Open logs", Some(sel!(openLogs:)), target));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = item(mtm, "Quit Glass", Some(sel!(quitGlass:)), target);
    quit.setKeyEquivalent(ns_string!("q"));
    quit.setToolTip(Some(ns_string!(
        "Stops Glass until you next log in (or run glass setup)"
    )));
    menu.addItem(&quit);
    menu
}

fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<objc2::runtime::Sel>,
    target: &MenuTarget,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            ns_string!(""),
        )
    };
    if action.is_some() {
        unsafe { item.setTarget(Some(target)) };
    }
    item
}
