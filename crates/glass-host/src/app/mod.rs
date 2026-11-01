//! The Glass app (`glass app`, or Glass.app): a native window for everything the CLI does,
//! from first-run setup to who's connected, links, machines, history, AI agents, settings
//! and teams. Drawn on the GPU with GPUI. It's its own process, separate from the
//! background service, and talks to the service the way the CLI does: the status file,
//! the control socket and the shared config files. One window; opening it again brings it
//! to the front.

mod actions;
mod agents;
#[cfg_attr(windows, path = "shortcut.rs")]
mod bundle;
mod data;
mod history;
mod home;
mod links;
mod machines;
mod prefs;
mod sessions;
mod setup;
mod support;
mod teams;
mod ui;

use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, Sizable, Theme, ThemeMode, TitleBar, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

#[cfg_attr(windows, allow(unused_imports))]
pub use bundle::{install as install_bundle, open as open_app};
use data::{Machine, Snapshot, Tailnet};
use ui::I;

/// How often the window re-reads the quick state (status, viewers, settings…).
const TICK: Duration = Duration::from_millis(1000);
/// And Tailscale's (it runs the tailscale CLI).
const SLOW_TICK: Duration = Duration::from_secs(10);

/// Makes this the only app window: if one is already open, asks it to come forward and
/// returns `None`; otherwise listens for later asks.
#[cfg(unix)]
fn single_instance() -> Option<tokio::sync::mpsc::UnboundedReceiver<()>> {
    let sock = crate::config_dir().join("app.sock");
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&sock) {
        use std::io::Write;
        let _ = s.write_all(b"show\n");
        return None;
    }
    let _ = std::fs::create_dir_all(crate::config_dir());
    let _ = std::fs::remove_file(&sock);
    let (show_tx, show_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    if let Ok(listener) = std::os::unix::net::UnixListener::bind(&sock) {
        std::thread::spawn(move || {
            for _ in listener.incoming().flatten() {
                let _ = show_tx.send(());
            }
        });
    }
    Some(show_rx)
}

/// The same with a named pipe per user (and config dir), which every session can reach.
#[cfg(windows)]
fn single_instance() -> Option<tokio::sync::mpsc::UnboundedReceiver<()>> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT};
    let mut name = crate::ipc::path();
    name.set_file_name(format!("{}-app", name.file_name()?.to_string_lossy()));
    if std::fs::OpenOptions::new().write(true).open(&name).is_ok() {
        return None;
    }
    let (show_tx, show_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let wide = windows::core::HSTRING::from(name.as_os_str());
    std::thread::spawn(move || loop {
        // SAFETY: one pipe instance at a time, closed after each connection.
        unsafe {
            let pipe = CreateNamedPipeW(&wide, PIPE_ACCESS_INBOUND, PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, 1, 0, 64, 0, None);
            if pipe.is_invalid() {
                return;
            }
            if ConnectNamedPipe(pipe, None).is_ok() || windows::Win32::Foundation::GetLastError() == windows::Win32::Foundation::ERROR_PIPE_CONNECTED {
                let _ = show_tx.send(());
            }
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
        }
    });
    Some(show_rx)
}

pub fn run(rt: tokio::runtime::Runtime) -> anyhow::Result<()> {
    // Already open: bring that window forward instead.
    let Some(show_rx) = single_instance() else { return Ok(()) };
    let handle = rt.handle().clone();
    // The runtime lives as long as the app; Tailscale's helpers need it.
    std::mem::forget(rt);

    gpui_kit::application().with_assets(ui::Assets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.activate(true);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None), KeyBinding::new("cmd-w", Close, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus([Menu { name: "Glass".into(), items: vec![MenuItem::action("Quit Glass", Quit)], disabled: false }]);

        let store = cx.new(|cx| Store::new(handle, cx));
        cx.set_global(StoreHandle(store.clone()));

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(980.), px(680.)), cx)),
            window_min_size: Some(size(px(780.), px(540.))),
            window_background: WindowBackgroundAppearance::Blurred,
            app_id: Some("dev.glassdesk.app".into()),
            ..TitleBar::window_options()
        };
        let (window, _) = gpui_kit::open_window(options, cx, |window, cx| {
            apply_theme(window, cx);
            cx.new(|cx| GlassApp::new(store, window, cx))
        })
        .expect("open the Glass window");

        // Another `glass app` asked for the window.
        let mut show_rx = show_rx;
        cx.spawn(async move |cx| {
            while show_rx.recv().await.is_some() {
                let _ = window.update(cx, |_, window, cx| {
                    cx.activate(true);
                    window.activate_window();
                });
            }
        })
        .detach();
    });
    Ok(())
}

actions!(glass, [Quit, Close]);

/// The quick-support window (`glass support`).
pub fn run_support(rt: tokio::runtime::Runtime, session: crate::support::Session) -> anyhow::Result<()> {
    support::run(rt, session)
}

/// Glass's look on top of the component theme: near-black glass in dark mode, paper in
/// light, one cool accent.
fn apply_theme(window: &mut Window, cx: &mut App) {
    let dark = matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark);
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, Some(window), cx);
    // Through `update`, so the components' tokens follow the colors.
    Theme::update(cx, |t| restyle(t, dark));
}

fn restyle(t: &mut Theme, dark: bool) {
    let c = |hex: u32| -> Hsla { rgb(hex).into() };
    let accent = if dark { c(0x5cb2ff) } else { c(0x0a6cdc) };
    t.primary = accent;
    t.primary_hover = accent.opacity(0.9);
    t.primary_active = accent.opacity(0.8);
    t.primary_foreground = if dark { c(0x04121f) } else { c(0xffffff) };
    t.button_primary = accent;
    t.button_primary_hover = accent.opacity(0.9);
    t.button_primary_active = accent.opacity(0.8);
    t.button_primary_foreground = t.primary_foreground;
    t.selection = accent.opacity(0.3);
    t.switch_thumb = c(0xffffff);
    t.ring = accent;
    t.link = accent;
    t.success = if dark { c(0x6fdc8c) } else { c(0x1f9d48) };
    t.danger = if dark { c(0xff7b7b) } else { c(0xd93636) };
    t.warning = if dark { c(0xf2c26b) } else { c(0xb7791f) };
    if dark {
        t.background = c(0x0e1013);
        t.foreground = c(0xeef0f3);
        t.muted_foreground = c(0x8f96a3);
        t.border = hsla(0., 0., 1., 0.08);
        t.input = hsla(0., 0., 1., 0.12);
        t.secondary = hsla(0., 0., 1., 0.05);
        t.secondary_hover = hsla(0., 0., 1., 0.09);
        t.secondary_active = hsla(0., 0., 1., 0.12);
        t.sidebar = hsla(220. / 360., 0.12, 0.06, 0.72);
    } else {
        t.background = c(0xfbfbfc);
        t.foreground = c(0x16181d);
        t.muted_foreground = c(0x6b7280);
        t.border = hsla(0., 0., 0., 0.08);
        t.input = hsla(0., 0., 0., 0.14);
        t.secondary = hsla(0., 0., 0., 0.035);
        t.secondary_hover = hsla(0., 0., 0., 0.06);
        t.secondary_active = hsla(0., 0., 0., 0.09);
        t.sidebar = hsla(220. / 360., 0.2, 0.96, 0.7);
    }
    t.font_size = px(14.);
}

// ---- Shared state ----

/// What the window shows, kept current in the background.
pub struct Store {
    pub snap: Snapshot,
    pub tailnet: Tailnet,
    pub machines: Vec<Machine>,
    /// The newest release, once checked.
    pub latest: Option<String>,
    /// Asking glassdesk.dev for the newest release right now.
    pub checking: bool,
    /// Installing an update right now.
    pub updating: bool,
    /// The first read has finished (until then, don't show setup or "not running").
    pub loaded: bool,
    rt: tokio::runtime::Handle,
}

const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(30 * 60);

struct StoreHandle(Entity<Store>);
impl Global for StoreHandle {}

impl Store {
    fn new(rt: tokio::runtime::Handle, cx: &mut Context<Self>) -> Self {
        // Quick state, every second.
        cx.spawn(async move |this, cx| {
            loop {
                let snap = cx.background_executor().spawn(async { data::gather() }).await;
                let Ok(()) = this.update(cx, |s, cx| s.set(snap, cx)) else { break };
                cx.background_executor().timer(TICK).await;
            }
        })
        .detach();
        // Tailscale, every few seconds.
        let rt2 = rt.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let rt = rt2.clone();
                let (tailnet, machines) = cx.background_executor().spawn(async move { rt.block_on(data::tailnet()) }).await;
                let Ok(()) = this.update(cx, |s, cx| {
                    if s.tailnet != tailnet || s.machines != machines {
                        s.tailnet = tailnet;
                        s.machines = machines;
                        cx.notify();
                    }
                }) else {
                    break;
                };
                cx.background_executor().timer(SLOW_TICK).await;
            }
        })
        .detach();
        // Updates: now, then every half hour (the window can stay open for days).
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(()) = this.update(cx, |s, cx| s.check_updates(cx)) else { break };
                cx.background_executor().timer(UPDATE_CHECK_EVERY).await;
            }
        })
        .detach();
        Self {
            snap: Snapshot::default(),
            tailnet: Tailnet::Checking,
            machines: Vec::new(),
            latest: None,
            checking: false,
            updating: false,
            loaded: false,
            rt,
        }
    }

    /// Asks glassdesk.dev for the newest release (in the background).
    pub fn check_updates(&mut self, cx: &mut Context<Self>) {
        if self.checking {
            return;
        }
        self.checking = true;
        cx.notify();
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let latest = cx.background_executor().spawn(async move { rt.block_on(crate::update::fetch_latest()) }).await;
            let _ = this.update(cx, |s, cx| {
                if latest.is_some() {
                    s.latest = latest;
                }
                s.checking = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn set(&mut self, snap: Snapshot, cx: &mut Context<Self>) {
        if !self.loaded || self.snap != snap {
            self.snap = snap;
            self.loaded = true;
            cx.notify();
        }
    }

    pub fn update_available(&self) -> Option<&str> {
        self.latest.as_deref().filter(|v| crate::update::newer(v, crate::update::CURRENT))
    }

    /// Re-reads right away (after an action).
    pub fn refresh(cx: &mut App) {
        let store = cx.global::<StoreHandle>().0.clone();
        cx.spawn(async move |cx| {
            let snap = cx.background_executor().spawn(async { data::gather() }).await;
            let _ = store.update(cx, |s, cx| s.set(snap, cx));
        })
        .detach();
    }

    /// Re-checks Tailscale right away.
    pub fn refresh_tailnet(cx: &mut App) {
        let store = cx.global::<StoreHandle>().0.clone();
        let rt = store.read(cx).rt.clone();
        cx.spawn(async move |cx| {
            let (tailnet, machines) = cx.background_executor().spawn(async move { rt.block_on(data::tailnet()) }).await;
            let _ = store.update(cx, |s, cx| {
                s.tailnet = tailnet;
                s.machines = machines;
                cx.notify();
            });
        })
        .detach();
    }

    pub fn runtime(cx: &App) -> tokio::runtime::Handle {
        cx.global::<StoreHandle>().0.read(cx).rt.clone()
    }
}

/// Runs a blocking action off the main thread, then shows its message (or error) and
/// re-reads the state.
pub fn act(window: &mut Window, cx: &mut App, f: impl FnOnce() -> anyhow::Result<String> + Send + 'static) {
    window
        .spawn(cx, async move |cx| {
            let result = cx.background_executor().spawn(async move { f() }).await;
            let _ = cx.update(|window, cx| {
                toast(window, cx, result);
                Store::refresh(cx);
            });
        })
        .detach();
}

pub fn toast(window: &mut Window, cx: &mut App, result: anyhow::Result<String>) {
    match result {
        Ok(msg) if msg.is_empty() => {}
        Ok(msg) => window.push_notification(Notification::success(msg), cx),
        Err(e) => window.push_notification(Notification::error(format!("{e:#}")), cx),
    }
}

// ---- The window ----

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Home,
    Sessions,
    Links,
    Machines,
    History,
    Agents,
    Settings,
    Teams,
}

impl Page {
    const ALL: [Page; 8] =
        [Page::Home, Page::Sessions, Page::Links, Page::Machines, Page::History, Page::Agents, Page::Settings, Page::Teams];

    fn label(self) -> &'static str {
        match self {
            Page::Home => "Home",
            Page::Sessions => "Connected",
            Page::Links => "Links",
            Page::Machines => "Machines",
            Page::History => "History",
            Page::Agents => "AI agents",
            Page::Settings => "Settings",
            Page::Teams => "Teams & SSO",
        }
    }

    fn icon(self) -> I {
        match self {
            Page::Home => I::House,
            Page::Sessions => I::MonitorSmartphone,
            Page::Links => I::Link,
            Page::Machines => I::Network,
            Page::History => I::ClockFading,
            Page::Agents => I::Bot,
            Page::Settings => I::Settings,
            Page::Teams => I::Building2,
        }
    }
}

/// Where an address on Home leads: this computer on your Tailscale network, or from anywhere.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Via {
    Tailnet,
    Anywhere,
}

pub struct GlassApp {
    store: Entity<Store>,
    page: Page,
    /// First-run setup, shown instead of the pages.
    setup: Option<setup::Setup>,
    chat: Entity<InputState>,
    /// The conversation's scroll, kept at the newest message.
    chat_scroll: ScrollHandle,
    /// How many chat messages have been seen on the Connected page (the rest are unread).
    chat_seen: usize,
    feedback: Entity<InputState>,
    token_input: Entity<InputState>,
    rule_input: Entity<InputState>,
    /// Text settings, by `glass config` key.
    fields: HashMap<&'static str, Entity<InputState>>,
    /// Notes for the agent, per question.
    notes: HashMap<u64, Entity<InputState>>,
    link: links::LinkForm,
    /// Which address Home shows, copies and puts in the QR code.
    via: Via,
    rule: teams::RuleForm,
    show_token: bool,
    /// MCP clients: (client, installed here, glass added).
    clients: Vec<(actions::Client, bool, bool)>,
    /// A token SCIM just generated, shown once.
    new_scim: Option<String>,
    _subs: Vec<Subscription>,
}

impl GlassApp {
    fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let chat = input("Message everyone viewing…", window, cx);
        let feedback = input("What happened, or what you'd like…", window, cx);
        let token_input = cx.new(|cx| InputState::new(window, cx).placeholder("A new token (16+ characters)").masked(true));
        let rule_input = input("name@example.com, @example.com, group:team or *", window, cx);
        let mut subs = vec![cx.observe(&store, |this, store, cx| {
            this.on_store(store, cx);
            cx.notify();
        })];
        subs.push(cx.subscribe_in(&chat, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.send_chat(window, cx);
            }
        }));
        let mut this = Self {
            store,
            page: Page::Home,
            setup: None,
            chat,
            chat_scroll: ScrollHandle::new(),
            chat_seen: 0,
            feedback,
            token_input,
            rule_input,
            fields: HashMap::new(),
            notes: HashMap::new(),
            link: links::LinkForm::default(),
            via: Via::Tailnet,
            rule: teams::RuleForm::default(),
            show_token: false,
            clients: Vec::new(),
            new_scim: None,
            _subs: subs,
        };
        this.make_fields(window, cx);
        // For screenshots and tests: start on a page, or in setup.
        match std::env::var("GLASS_APP_PAGE").as_deref() {
            Ok(name) if name.starts_with("setup") => this.setup = Some(setup::Setup::at(name.trim_start_matches("setup-"))),
            Ok(name) => {
                if let Some(p) = Page::ALL.into_iter().find(|p| p.label().to_lowercase().starts_with(name)) {
                    this.go(p, cx);
                }
            }
            Err(_) => {}
        }
        // While setup waits on Tailscale, check it every couple of seconds.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let Ok(waiting) = this.update(cx, |this, _| this.setup.as_ref().is_some_and(|s| s.step == setup::Step::Tailscale)) else { break };
                if waiting {
                    let _ = cx.update(Store::refresh_tailnet);
                }
            }
        })
        .detach();
        cx.observe_window_appearance(window, |_, window, cx| {
            apply_theme(window, cx);
            cx.notify();
        })
        .detach();
        this
    }

    fn on_store(&mut self, store: Entity<Store>, cx: &mut Context<Self>) {
        let s = store.read(cx);
        // First run (never set up), once the first read is in: setup takes the window.
        if s.loaded && self.setup.is_none() && !s.snap.installed && !setup::dismissed() {
            self.setup = Some(setup::Setup::default());
        }
        let asks: Vec<u64> = s.snap.agent.asks.iter().map(|a| a.id).collect();
        self.notes.retain(|id, _| asks.contains(id));
    }

    fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        if page == Page::Agents {
            self.check_clients(cx);
        }
        cx.notify();
    }

    /// The bottom of the sidebar: update now when there's a new version, else check.
    fn update_button(&self, update: Option<String>, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let (checking, updating, checked) = (store.checking, store.updating, store.latest.is_some());
        let theme = cx.theme();
        let button = match &update {
            Some(v) => Button::new("update")
                .primary()
                .small()
                .w_full()
                .icon(I::Download)
                .label(if updating { "Updating…".to_string() } else { format!("Update to {v}") })
                .loading(updating)
                .disabled(updating)
                .on_click(cx.listener(|this, _, window, cx| this.update_now(window, cx))),
            None => Button::new("check-updates")
                .ghost()
                .small()
                .w_full()
                .icon(I::RefreshCw)
                .label(if checking { "Checking…" } else { "Check for updates" })
                .loading(checking)
                .disabled(checking)
                .on_click(cx.listener(|this, _, _, cx| this.store.update(cx, |s, cx| s.check_updates(cx)))),
        };
        v_flex()
            .mx_1()
            .mb_1()
            .gap_1p5()
            .child(button)
            .child(
                div()
                    .px_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(match &update {
                        Some(_) => format!("You have {}", crate::update::CURRENT),
                        None if checked && !checking => format!("Glass {} · up to date", crate::update::CURRENT),
                        None => format!("Glass {}", crate::update::CURRENT),
                    }),
            )
    }

    /// Installs the newest release, restarts the service on it, and reopens this window
    /// on the new version.
    pub(crate) fn update_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).updating {
            return;
        }
        self.store.update(cx, |s, cx| {
            s.updating = true;
            cx.notify();
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async { actions::update_now() }).await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(version) => {
                    toast(window, cx, Ok(format!("Updated to {version}. Reopening…")));
                    actions::relaunch();
                    cx.quit();
                }
                Err(e) => {
                    this.store.update(cx, |s, cx| {
                        s.updating = false;
                        cx.notify();
                    });
                    toast(window, cx, Err(e));
                }
            });
        })
        .detach();
    }

    fn send_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.chat.read(cx).value().to_string();
        self.chat.update(cx, |s, cx| s.set_value("", window, cx));
        act(window, cx, move || actions::chat(&text));
    }
}

impl Render for GlassApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(setup) = self.setup.clone() {
            return self.render_setup(setup, window, cx).into_any_element();
        }
        let page = match self.page {
            Page::Home => self.home(window, cx).into_any_element(),
            Page::Sessions => self.sessions(window, cx).into_any_element(),
            Page::Links => self.links(window, cx).into_any_element(),
            Page::Machines => self.machines(window, cx).into_any_element(),
            Page::History => self.history(window, cx).into_any_element(),
            Page::Agents => self.agents(window, cx).into_any_element(),
            Page::Settings => self.settings(window, cx).into_any_element(),
            Page::Teams => self.teams(window, cx).into_any_element(),
        };
        h_flex()
            .size_full()
            .text_color(cx.theme().foreground)
            .child(self.sidebar(cx))
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .bg(cx.theme().background)
                    .border_l_1()
                    .border_color(cx.theme().border)
                    // Room for the (transparent) title bar, which drags the window.
                    .child(TitleBar::new().bg(cx.theme().background).border_b_0())
                    .child(div().id("page").flex_1().min_h_0().overflow_y_scroll().child(page)),
            )
            .into_any_element()
    }
}

impl GlassApp {
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let snap = &store.snap;
        let theme = cx.theme();
        let (state, color) = if !store.loaded {
            ("Checking…", theme.muted_foreground)
        } else if !snap.running() {
            ("Off", theme.danger)
        } else if !snap.viewers.is_empty() {
            ("In use", theme.primary)
        } else if snap.permissions() != (true, true) {
            ("Needs attention", theme.warning)
        } else {
            ("Ready", theme.success)
        };
        let badge = |page: Page| -> Option<usize> {
            let n = match page {
                Page::Sessions => {
                    let unread = snap.chat.iter().skip(self.chat_seen).filter(|m| !m.mine).count();
                    snap.viewers.len() + snap.pending.len() + unread
                }
                Page::Agents => snap.agent.asks.len(),
                _ => 0,
            };
            (n > 0).then_some(n)
        };
        let update = store.update_available().map(str::to_string);
        v_flex()
            .w(px(216.))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .pt(px(44.))
            .px_3()
            .pb_3()
            .gap_1()
            .child(
                h_flex()
                    .px_2()
                    .pb_4()
                    .gap_2p5()
                    .child(ui::logo(28.))
                    .child(
                        v_flex()
                            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("Glass"))
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(ui::dot(color))
                                    .child(state),
                            ),
                    ),
            )
            .children(Page::ALL.into_iter().map(|page| {
                let active = self.page == page;
                let theme = cx.theme();
                h_flex()
                    .id(SharedString::from(format!("nav-{}", page.label())))
                    .h(px(32.))
                    .px_2p5()
                    .gap_2p5()
                    .rounded(px(8.))
                    .text_sm()
                    .cursor_pointer()
                    .when(active, |d| d.bg(theme.secondary_active).font_weight(FontWeight::MEDIUM))
                    .when(!active, |d| d.text_color(theme.foreground.opacity(0.82)).hover(|d| d.bg(theme.secondary_hover)))
                    .child(Icon::new(page.icon()).small().text_color(if active { theme.primary } else { theme.muted_foreground }))
                    .child(div().flex_1().child(page.label()))
                    .children(badge(page).map(|n| ui::count(n, theme.primary, theme.primary_foreground)))
                    .on_click(cx.listener(move |this, _, _, cx| this.go(page, cx)))
            }))
            .child(div().flex_1())
            .child(self.update_button(update, cx))
    }
}
