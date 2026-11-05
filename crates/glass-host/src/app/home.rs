//! Home: is Glass ready, where to connect (with a QR code), anything that needs fixing,
//! and the switches people reach for most.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::data::Tailnet;
use super::ui::{self, I};
use super::{GlassApp, Page, Via, act, actions};
use crate::permissions;

impl GlassApp {
    pub(super) fn home(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let snap = store.snap.clone();
        let tailnet = store.tailnet.clone();
        let loaded = store.loaded;
        let theme = cx.theme().clone();
        let (screen, input) = snap.permissions();
        // The address shown, copied and put in the QR code: on your tailnet, or from anywhere
        // (only while remote access is on).
        let me = cx.entity();
        let via = if self.via == Via::Anywhere && snap.anywhere_url.is_some() { Via::Anywhere } else { Via::Tailnet };
        let url = match via {
            Via::Anywhere => snap.anywhere_url.clone(),
            Via::Tailnet => snap.url(),
        };

        let (title, line, tint) = if !loaded {
            ("Checking…", String::new(), theme.muted_foreground)
        } else if !snap.running() {
            (
                "Glass is off",
                if snap.installed { "The background service isn't running." } else { "Turn it on to connect from your other devices." }.to_string(),
                theme.danger,
            )
        } else if !screen {
            ("Glass can't see this screen yet", "Allow Screen Recording below, then you're set.".to_string(), theme.warning)
        } else if !snap.viewers.is_empty() {
            (
                "In use",
                match snap.viewers.len() {
                    1 => format!("{} is connected.", snap.viewers[0].describe),
                    n => format!("{n} devices are connected."),
                },
                theme.primary,
            )
        } else {
            (
                "Ready for connections",
                match via {
                    Via::Tailnet => "Open the address on any device signed in to your Tailscale.",
                    Via::Anywhere => "Open the address in any browser, then sign in.",
                }
                .to_string(),
                theme.success,
            )
        };

        // The hero: status, address and a QR code to scan.
        let hero = h_flex()
            .p_5()
            .gap_6()
            .rounded(px(16.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(ui::status(
                        match () {
                            _ if !loaded => "Checking",
                            _ if !snap.running() => "Off",
                            _ if !snap.viewers.is_empty() => "Live",
                            _ => "On",
                        },
                        tint,
                    ))
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(line))
                    .when_some(url.clone().filter(|_| snap.running()), |d, url| {
                        let copy = url.clone();
                        let open = url.clone();
                        d.child(
                            h_flex()
                                .mt_1()
                                .gap_2()
                                .child(address_switch(via, snap.remote, me.clone()))
                                .child(
                                    ui::mono(url)
                                        .px_2p5()
                                        .py_1p5()
                                        .rounded(px(7.))
                                        .bg(theme.background)
                                        .border_1()
                                        .border_color(theme.border)
                                        .truncate(),
                                )
                                .child(Button::new("copy-url").icon(I::Copy).ghost().small().tooltip("Copy the address").on_click(
                                    move |_, window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                        super::toast(window, cx, Ok("Address copied.".into()));
                                    },
                                ))
                                .child(Button::new("open-url").icon(I::ExternalLink).ghost().small().tooltip("Open in your browser").on_click(
                                    move |_, _, _| actions::open(&open),
                                )),
                        )
                    })
                    .when(loaded && !snap.running(), |d| {
                        let installed = snap.installed;
                        d.child(
                            h_flex().mt_1().child(Button::new("turn-on").primary().icon(I::Power).label("Turn on Glass").on_click(
                                move |_, window, cx| {
                                    act(window, cx, move || if installed { actions::restart() } else { actions::install(true) })
                                },
                            )),
                        )
                    }),
            )
            .when_some(url.filter(|_| snap.running() && screen), |d, url| {
                d.child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .child(ui::qr(&url, 132.))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(match via {
                            Via::Tailnet => "Scan with your iPad",
                            Via::Anywhere => "Scan, then sign in",
                        })),
                )
            });

        // What still needs doing.
        let mut fixes = Vec::new();
        if loaded && snap.running() {
            if !screen {
                fixes.push(ui::icon_row(
                    I::Monitor,
                    theme.warning,
                    "Allow Screen Recording",
                    "So Glass can show this screen. macOS may ask to quit and reopen Glass; that's fine.",
                    h_flex()
                        .gap_2()
                        .child(Button::new("fix-screen").primary().small().label("Allow…").on_click(|_, _, _| {
                            permissions::open_pane(permissions::SCREEN_RECORDING_PANE)
                        }))
                        .child(Button::new("restart-screen").ghost().small().label("Restart Glass").on_click(|_, window, cx| {
                            act(window, cx, actions::restart)
                        })),
                    cx,
                ));
            }
            if !input {
                fixes.push(ui::icon_row(
                    I::MousePointerClick,
                    theme.warning,
                    "Allow Accessibility",
                    "So your devices can use this computer's mouse and keyboard.",
                    Button::new("fix-input")
                        .primary()
                        .small()
                        .label("Allow…")
                        .on_click(|_, _, _| permissions::open_pane(permissions::ACCESSIBILITY_PANE)),
                    cx,
                ));
            }
        }
        match &tailnet {
            Tailnet::Missing => fixes.push(ui::icon_row(
                I::Network,
                theme.muted_foreground,
                "Tailscale isn't installed",
                "It's free and links your devices privately. Without it, use one-time links.",
                Button::new("get-ts").small().label("Get Tailscale").on_click(|_, _, _| actions::open("https://tailscale.com/download/mac")),
                cx,
            )),
            Tailnet::NotRunning | Tailnet::LoggedOut => fixes.push(ui::icon_row(
                I::Network,
                theme.warning,
                if tailnet == Tailnet::NotRunning { "Tailscale isn't running" } else { "Tailscale is signed out" },
                "Open Tailscale and sign in to reach this computer from your devices.",
                Button::new("open-ts").small().label("Open Tailscale").on_click(|_, _, _| actions::open_tailscale()),
                cx,
            )),
            Tailnet::NoHttps(_) => fixes.push(ui::icon_row(
                I::Lock,
                theme.warning,
                "Turn on HTTPS for your tailnet",
                "Browsers need it for Glass. One switch at the bottom of Tailscale's DNS page.",
                Button::new("https").small().label("Open DNS settings").on_click(|_, _, _| actions::open("https://login.tailscale.com/admin/dns")),
                cx,
            )),
            _ => {}
        }
        for ask in snap.agent.asks.iter().take(1) {
            fixes.insert(
                0,
                ui::icon_row(
                    I::Bot,
                    theme.primary,
                    if ask.consent {
                        "An agent asks to use this computer"
                    } else if ask.help {
                        "An agent needs your help"
                    } else {
                        "An agent asks before it acts"
                    },
                    // A consent's request id isn't for people.
                    ask.text.rsplit_once(" [").filter(|_| ask.consent).map_or(ask.text.clone(), |(t, _)| t.to_string()),
                    Button::new("see-ask").primary().small().label("Answer").on_click(cx.listener(|this, _, _, cx| this.go(Page::Agents, cx))),
                    cx,
                ),
            );
        }
        for p in snap.pending.iter().take(3) {
            let id = p.id;
            fixes.insert(
                0,
                ui::icon_row(
                    I::Hand,
                    theme.primary,
                    "Wants to connect",
                    p.who.clone(),
                    h_flex()
                        .gap_2()
                        .child(Button::new(("deny", id)).ghost().small().label("Deny").on_click(move |_, window, cx| {
                            act(window, cx, move || actions::decide(id, false))
                        }))
                        .child(Button::new(("allow", id)).primary().small().label("Allow").on_click(move |_, window, cx| {
                            act(window, cx, move || actions::decide(id, true))
                        })),
                    cx,
                ),
            );
        }

        let s = &snap.settings;
        let quick = vec![
            ui::icon_row(
                I::EyeOff,
                theme.primary,
                "Privacy screen",
                ui::managed(
                    if snap.viewers.is_empty() { "Blank this screen while you're connected (applies to the next session)." } else { "Blank this screen right now, while you're connected." },
                    snap.policy.locked("privacy"),
                ),
                Switch::new("privacy").checked(if snap.viewers.is_empty() { s.privacy } else { snap.privacy }).disabled(snap.policy.locked("privacy")).on_click({
                    let live = !snap.viewers.is_empty();
                    move |on: &bool, window, cx| {
                        let on = *on;
                        act(window, cx, move || if live { actions::privacy(on) } else { actions::set_setting("privacy", if on { "on" } else { "off" }) })
                    }
                }),
                cx,
            ),
            ui::icon_row(
                I::Hand,
                theme.primary,
                "Ask before guests connect",
                ui::managed(format!("Someone with a link waits until you allow them, here or in the {}.", super::ICON_PLACE), snap.policy.locked("ask-guests")),
                Switch::new("ask-guests").checked(s.ask_guests).disabled(snap.policy.locked("ask-guests")).on_click(|on: &bool, window, cx| {
                    let v = if *on { "on" } else { "off" };
                    act(window, cx, move || actions::set_setting("ask-guests", v))
                }),
                cx,
            ),
            ui::icon_row(
                I::Globe,
                theme.primary,
                "Reach from anywhere",
                ui::managed("Links can work outside your tailnet, end-to-end encrypted, with no open ports.", snap.policy.locked("remote")),
                Switch::new("remote").checked(snap.remote).disabled(snap.policy.locked("remote")).on_click(|on: &bool, window, cx| {
                    let on = *on;
                    act(window, cx, move || actions::set_remote(on))
                }),
                cx,
            ),
            ui::icon_row(
                I::Link,
                theme.primary,
                "Let someone in",
                "Make a one-time link with limits: how long, view only, files, clipboard.",
                Button::new("make-link")
                    .small()
                    .label("Make a link")
                    .on_click(cx.listener(|this, _, _, cx| this.go(Page::Links, cx))),
                cx,
            ),
        ];
        // Until it's installed: your devices' microphones on this computer need a driver.
        let mut quick = quick;
        if snap.mic.is_none() {
            quick.push(ui::icon_row(
                I::Mic,
                theme.primary,
                "Microphone from your devices",
                if cfg!(windows) {
                    "Talk into this computer's apps from your iPad or phone. Needs VB-CABLE, a free virtual audio cable."
                } else {
                    "Talk into this Mac's apps from your iPad or phone. Needs a small audio driver."
                },
                Button::new("home-install-mic")
                    .small()
                    .icon(I::Download)
                    .label("Install…")
                    .on_click(|_, window, cx| act(window, cx, || crate::mic::install(crate::mic::Ask::Dialog))),
                cx,
            ));
        }

        let recent: Vec<AnyElement> = snap.log.iter().take(4).map(|e| super::history::entry_row(e, cx)).collect();

        ui::page("Home", machine_line(), cx)
            .child(hero)
            .when(!fixes.is_empty(), |d| d.child(ui::section("Needs you", fixes, cx)))
            .child(ui::section("Quick settings", quick, cx))
            .when(!recent.is_empty(), |d| d.child(ui::section("Recent", recent, cx)))
    }
}

/// "Studio Mac · macOS 15.4", read once.
fn machine_line() -> &'static str {
    static LINE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LINE.get_or_init(|| format!("{} · {}", hostname(), crate::platform::os_version()))
}

pub fn hostname() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "This computer".into())
    })
}

/// The button beside the address: its icon says which address this is, and its menu switches
/// between your Tailscale network and from anywhere (turning remote access on if it's off).
fn address_switch(via: Via, remote_on: bool, me: Entity<GlassApp>) -> impl IntoElement {
    let icon = match via {
        Via::Tailnet => ui::tailscale(),
        Via::Anywhere => I::Globe.into(),
    };
    Button::new("address-via")
        .icon(icon)
        .ghost()
        .small()
        .dropdown_caret(true)
        .tooltip("Choose the address")
        .dropdown_menu(move |menu, _, _| {
            let tailnet = me.clone();
            let anywhere = me.clone();
            menu.min_w(px(230.))
                .item(PopupMenuItem::new("Your Tailscale network").icon(ui::tailscale()).checked(via == Via::Tailnet).on_click(
                    move |_, _, cx| {
                        tailnet.update(cx, |this, cx| {
                            this.via = Via::Tailnet;
                            cx.notify();
                        })
                    },
                ))
                .item(
                    PopupMenuItem::new(if remote_on { "From anywhere" } else { "From anywhere (off: turn it on)" })
                        .icon(I::Globe)
                        .checked(via == Via::Anywhere)
                        .on_click(move |_, window, cx| {
                            if !remote_on {
                                act(window, cx, move || actions::set_remote(true));
                            }
                            anywhere.update(cx, |this, cx| {
                                this.via = Via::Anywhere;
                                cx.notify();
                            })
                        }),
                )
        })
}
