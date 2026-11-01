//! First-run setup, the window's version of `glass setup`: Tailscale, the background
//! service, the two macOS permissions, then connecting a device. Each step checks itself
//! as you go, so there's rarely anything to click but "Continue".

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable, ActiveTheme, Icon, Sizable, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::data::Tailnet;
use super::ui::{self, I};
use super::{GlassApp, Store, actions};
use crate::permissions;

/// Screen Recording often applies only to a fresh process: restart this often while
/// waiting, at most a few times (each launch can show the system prompt again).
const NUDGE_EVERY: Duration = Duration::from_secs(8);
const MAX_NUDGES: u32 = 3;
/// A device let in by setup's link keeps access a week: it's the owner's own iPad.
const SETUP_GRANT: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Clone, Copy, PartialEq)]
pub enum Step {
    Welcome,
    Tailscale,
    Service,
    Permissions,
    Connect,
}

const STEPS: [Step; 5] = [Step::Welcome, Step::Tailscale, Step::Service, Step::Permissions, Step::Connect];

#[derive(Clone)]
pub struct Setup {
    pub step: Step,
    at_login: bool,
    busy: bool,
    /// Asked for Screen Recording: restart the service until it applies.
    nudging: bool,
    link: Option<(String, String)>,
}

impl Default for Setup {
    fn default() -> Self {
        Self { step: Step::Welcome, at_login: true, busy: false, nudging: false, link: None }
    }
}

impl Setup {
    /// Setup at a step by name (for screenshots and tests).
    pub fn at(name: &str) -> Self {
        let step = match name {
            "tailscale" => Step::Tailscale,
            "service" => Step::Service,
            "permissions" => Step::Permissions,
            "connect" => Step::Connect,
            _ => Step::Welcome,
        };
        Self { step, ..Self::default() }
    }
}

fn marker() -> std::path::PathBuf {
    crate::config_dir().join("app-setup-done")
}

/// Setup was finished or skipped once; don't offer it again on launch.
pub fn dismissed() -> bool {
    marker().exists()
}

impl GlassApp {
    fn setup_mut(&mut self) -> Option<&mut Setup> {
        self.setup.as_mut()
    }

    fn step_to(&mut self, step: Step, cx: &mut Context<Self>) {
        if let Some(s) = self.setup_mut() {
            s.step = step;
            s.busy = false;
        }
        if step == Step::Tailscale {
            Store::refresh_tailnet(cx);
        }
        cx.notify();
    }

    fn finish_setup(&mut self, cx: &mut Context<Self>) {
        let _ = std::fs::create_dir_all(crate::config_dir());
        let _ = std::fs::write(marker(), b"");
        self.setup = None;
        cx.notify();
    }

    /// Runs `f` off the main thread with the step's button spinning; `then` gets the result.
    fn setup_task<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
        then: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if let Some(s) = self.setup_mut() {
            s.busy = true;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let r = cx.background_executor().spawn(async move { f() }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(s) = this.setup_mut() {
                    s.busy = false;
                }
                match r {
                    Ok(v) => then(this, v, window, cx),
                    Err(e) => super::toast(window, cx, Err(e)),
                }
                Store::refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// After "Allow…" for Screen Recording: restart the service a few times until the
    /// grant takes.
    fn nudge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.setup.as_ref().is_some_and(|s| s.nudging) {
            return;
        }
        if let Some(s) = self.setup_mut() {
            s.nudging = true;
        }
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..MAX_NUDGES {
                cx.background_executor().timer(NUDGE_EVERY).await;
                let Ok(granted) = this.update(cx, |this, cx| this.store.read(cx).snap.permissions().0) else { return };
                if granted {
                    break;
                }
                let _ = cx.background_executor().spawn(async { crate::agent::restart() }).await;
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(s) = this.setup_mut() {
                    s.nudging = false;
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_setup(&mut self, setup: Setup, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let index = STEPS.iter().position(|s| *s == setup.step).unwrap_or(0);
        let body = match setup.step {
            Step::Welcome => self.welcome(cx).into_any_element(),
            Step::Tailscale => self.tailscale_step(&setup, cx).into_any_element(),
            Step::Service => self.service_step(&setup, cx).into_any_element(),
            Step::Permissions => self.permissions_step(&setup, cx).into_any_element(),
            Step::Connect => self.connect_step(&setup, window, cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new().bg(theme.background).border_b_0())
            .child(
                div().id("setup").flex_1().min_h_0().overflow_y_scroll().child(
                    v_flex()
                        .w_full()
                        .items_center()
                        .px_8()
                        .pt_6()
                        .pb_10()
                        .child(
                            v_flex()
                                .w_full()
                                .max_w(px(560.))
                                .gap_6()
                                // Progress: one bar per step.
                                .child(h_flex().gap_1p5().when(index > 0, |d| {
                                    d.children(STEPS[1..].iter().enumerate().map(|(i, _)| {
                                        div().h(px(3.)).flex_1().rounded_full().bg(if i < index { theme.primary } else { theme.border })
                                    }))
                                }))
                                .child(body),
                        ),
                ),
            )
    }

    fn welcome(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .items_center()
            .text_center()
            .gap_5()
            .pt(px(36.))
            .child(ui::logo(96.))
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_3xl().font_weight(FontWeight::BOLD).child("Welcome to Glass"))
                    .child(div().text_base().text_color(theme.muted_foreground).child("This computer, from your iPad or any browser. Smooth, private, no port forwarding.")),
            )
            .child(
                v_flex()
                    .w_full()
                    .mt_2()
                    .gap_3()
                    .p_5()
                    .rounded(px(16.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .children(
                        [
                            (I::Network, "Private by default", "Your devices find each other over Tailscale. Nothing is exposed to the internet."),
                            (I::Zap, "Built for low latency", "Hardware video at up to 120 fps, with sharp text when you stop moving."),
                            (I::ShieldCheck, "You stay in charge", "Links with limits, approvals, and one switch to end every session."),
                        ]
                        .map(|(icon, title, line)| {
                            h_flex()
                                .gap_3()
                                .text_left()
                                .child(ui::tile(icon, theme.primary))
                                .child(
                                    v_flex()
                                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                                        .child(div().text_xs().text_color(theme.muted_foreground).child(line)),
                                )
                        }),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Button::new("start")
                            .primary()
                            .w_full()
                            .label("Set up Glass")
                            .on_click(cx.listener(|this, _, _, cx| this.step_to(Step::Tailscale, cx))),
                    )
                    .child(
                        Button::new("skip-setup")
                            .ghost()
                            .w_full()
                            .label("I'll do it later")
                            .on_click(cx.listener(|this, _, _, cx| this.finish_setup(cx))),
                    ),
            )
    }

    fn step_header(n: usize, title: &str, line: &str, cx: &App) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(cx.theme().primary).child(format!("STEP {n} OF 4")))
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(line.to_string()))
    }

    fn tailscale_step(&mut self, setup: &Setup, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tailnet = self.store.read(cx).tailnet.clone();
        let (icon, tint, title, line): (I, Hsla, String, String) = match &tailnet {
            Tailnet::Checking => (I::LoaderCircle, theme.muted_foreground, "Checking for Tailscale…".into(), String::new()),
            Tailnet::Missing => (
                I::Download,
                theme.warning,
                "Install Tailscale".into(),
                "It's free. Install it here and on your iPad, and sign in to both with the same account.".into(),
            ),
            Tailnet::NotRunning => (I::Power, theme.warning, "Open Tailscale".into(), "It's installed but not running.".into()),
            Tailnet::LoggedOut => (I::KeyRound, theme.warning, "Sign in to Tailscale".into(), "Click its icon in the menu bar and log in.".into()),
            Tailnet::NoHttps(_) => (
                I::Lock,
                theme.warning,
                "Turn on HTTPS certificates".into(),
                "Browsers need HTTPS for Glass. It's one switch at the bottom of Tailscale's DNS page.".into(),
            ),
            Tailnet::Ready(name) => (I::CircleCheck, theme.success, "Tailscale is ready".into(), name.clone()),
        };
        let action = match &tailnet {
            Tailnet::Missing => Some(Button::new("ts-get").label("Get Tailscale").icon(I::ExternalLink).on_click(|_, _, _| {
                actions::open(actions::TAILSCALE_DOWNLOAD)
            })),
            Tailnet::NotRunning | Tailnet::LoggedOut => Some(Button::new("ts-open").label("Open Tailscale").on_click(|_, _, _| actions::open_tailscale())),
            Tailnet::NoHttps(_) => Some(Button::new("ts-https").label("Open DNS settings").icon(I::ExternalLink).on_click(|_, _, _| {
                actions::open("https://login.tailscale.com/admin/dns")
            })),
            _ => None,
        };
        let ready = tailnet.name().is_some();
        v_flex()
            .gap_5()
            .child(Self::step_header(1, "Connect your devices", "Glass uses Tailscale to reach this computer securely from your other devices.", cx))
            .child(ui::group(
                vec![
                    h_flex()
                        .p_4()
                        .gap_3()
                        .child(ui::tile(icon, tint))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                                .child(div().text_xs().text_color(theme.muted_foreground).child(line)),
                        )
                        .children(action.map(|b| b.small()))
                        .into_any_element(),
                ],
                cx,
            ))
            .when(!ready && tailnet != Tailnet::Checking, |d| {
                d.child(div().text_xs().text_color(theme.muted_foreground).child("This updates by itself once it's done."))
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Button::new("ts-skip")
                            .ghost()
                            .label("Skip: connect with a link instead")
                            .on_click(cx.listener(|this, _, _, cx| this.step_to(Step::Service, cx))),
                    )
                    .child(
                        Button::new("ts-next")
                            .primary()
                            .label("Continue")
                            .disabled(!ready)
                            .loading(setup.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let rt = Store::runtime(cx);
                                this.setup_task(
                                    window,
                                    cx,
                                    move || {
                                        rt.block_on(crate::tailscale::ensure_cert(&crate::config_dir().join("certs")))
                                            .map(|_| ())
                                            .ok_or_else(|| anyhow::anyhow!("Couldn't get a certificate from Tailscale. Check HTTPS is on, then try again."))
                                    },
                                    |this, (), _, cx| this.step_to(Step::Service, cx),
                                );
                            })),
                    ),
            )
    }

    fn service_step(&mut self, setup: &Setup, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let snap = self.store.read(cx).snap.clone();
        let running = snap.running();
        v_flex()
            .gap_5()
            .child(Self::step_header(2, "Turn on Glass", "Glass runs quietly in the background, with an icon in the menu bar when someone's connected.", cx))
            .child(ui::group(
                vec![
                    ui::icon_row(
                        if running { I::CircleCheck } else { I::Power },
                        if running { theme.success } else { theme.primary },
                        if running { "Glass is running" } else { "Background service" },
                        match &snap.status {
                            Some(st) => format!("Version {}", st.version),
                            None => "Not running yet".into(),
                        },
                        div(),
                        cx,
                    ),
                    ui::row(
                        "Start when I log in",
                        "So this computer is always reachable.",
                        Switch::new("at-login").checked(setup.at_login).on_click(cx.listener(|this, on: &bool, _, cx| {
                            if let Some(s) = this.setup_mut() {
                                s.at_login = *on;
                            }
                            cx.notify();
                        })),
                        cx,
                    ),
                ],
                cx,
            ))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .when(running, |d| {
                        d.child(Button::new("svc-next").primary().label("Continue").on_click(cx.listener(|this, _, _, cx| this.step_to(Step::Permissions, cx))))
                    })
                    .when(!running, |d| {
                        let at_login = setup.at_login;
                        d.child(
                            Button::new("svc-on")
                                .primary()
                                .icon(I::Power)
                                .label("Turn on Glass")
                                .loading(setup.busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.setup_task(window, cx, move || actions::install(at_login), |this, _, _, cx| this.step_to(Step::Permissions, cx));
                                })),
                        )
                    }),
            )
    }

    fn permissions_step(&mut self, setup: &Setup, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let snap = self.store.read(cx).snap.clone();
        let (screen, input) = snap.permissions();
        let state = |ok: bool, waiting: bool| -> AnyElement {
            if ok {
                ui::status("Allowed", theme.success).into_any_element()
            } else if waiting {
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(I::LoaderCircle).xsmall())
                    .child("Waiting…")
                    .into_any_element()
            } else {
                div().into_any_element()
            }
        };
        v_flex()
            .gap_5()
            .child(Self::step_header(
                3,
                "Allow two permissions",
                "macOS asks you to approve these yourself. Switch Glass (\"glass\") on in each list that opens.",
                cx,
            ))
            .child(ui::group(
                vec![
                    ui::icon_row(
                        I::Monitor,
                        if screen { theme.success } else { theme.primary },
                        "Screen Recording",
                        "So Glass can show this screen. If macOS offers to quit and reopen it, say yes.",
                        h_flex().gap_2().child(state(screen, setup.nudging)).when(!screen, |d| {
                            d.child(Button::new("allow-screen").small().primary().label("Allow…").on_click(cx.listener(|this, _, window, cx| {
                                permissions::open_pane(permissions::SCREEN_RECORDING_PANE);
                                this.nudge(window, cx);
                            })))
                        }),
                        cx,
                    ),
                    ui::icon_row(
                        I::MousePointerClick,
                        if input { theme.success } else { theme.primary },
                        "Accessibility",
                        "So your devices can use the mouse and keyboard here.",
                        h_flex().gap_2().child(state(input, false)).when(!input, |d| {
                            d.child(Button::new("allow-input").small().primary().label("Allow…").on_click(|_, _, _| {
                                permissions::open_pane(permissions::ACCESSIBILITY_PANE)
                            }))
                        }),
                        cx,
                    ),
                ],
                cx,
            ))
            .when(!screen, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Glass isn't listed? Click + in the list and choose glass in your home folder's .glass/bin."),
                )
            })
            .child(
                h_flex()
                    .justify_between()
                    .child(Button::new("perm-skip").ghost().label("Skip for now").on_click(cx.listener(|this, _, _, cx| this.step_to(Step::Connect, cx))))
                    .child(
                        Button::new("perm-next")
                            .primary()
                            .label("Continue")
                            .disabled(!(screen && input))
                            .on_click(cx.listener(|this, _, _, cx| this.step_to(Step::Connect, cx))),
                    ),
            )
    }

    fn connect_step(&mut self, setup: &Setup, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let store = self.store.read(cx);
        let tailnet_url = store.tailnet.name().and(store.snap.url());
        let link = setup.link.clone();
        let shown = tailnet_url.clone().or_else(|| link.as_ref().map(|(u, _)| u.clone()));
        v_flex()
            .gap_5()
            .child(Self::step_header(
                4,
                "Connect your iPad",
                if tailnet_url.is_some() {
                    "Scan this with the iPad's camera, or open the address on any device signed in to your Tailscale."
                } else {
                    "Without Tailscale, a one-time link connects any device, from any network, end-to-end encrypted."
                },
                cx,
            ))
            .child(match shown {
                Some(url) => {
                    let copy = url.clone();
                    h_flex()
                        .p_5()
                        .gap_5()
                        .rounded(px(16.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.secondary)
                        .child(ui::qr(&url, 168.))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_3()
                                .child(ui::mono(url).p_2().rounded(px(7.)).bg(theme.background).border_1().border_color(theme.border).truncate())
                                .child(div().text_xs().text_color(theme.muted_foreground).child(match &link {
                                    Some((_, summary)) if tailnet_url.is_none() => summary.clone(),
                                    _ => "Tip: in Safari, Share › Add to Home Screen opens Glass full screen.".into(),
                                }))
                                .child(h_flex().child(Button::new("copy-setup-url").small().icon(I::Copy).label("Copy").on_click(move |_, window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    super::toast(window, cx, Ok("Copied.".into()));
                                }))),
                        )
                        .into_any_element()
                }
                None => ui::group(
                    vec![ui::icon_row(
                        I::Link,
                        theme.primary,
                        "A link for your iPad",
                        "Works on one device, then it keeps access for a week.",
                        Button::new("setup-link").small().primary().label("Make link").loading(setup.busy).on_click(cx.listener(|this, _, window, cx| {
                            this.setup_task(
                                window,
                                cx,
                                || actions::link(SETUP_GRANT, true, crate::access::Scope::default()),
                                |this, made, _, cx| {
                                    if let Some(s) = this.setup_mut() {
                                        s.link = Some(made);
                                    }
                                    cx.notify();
                                },
                            );
                        })),
                        cx,
                    )],
                    cx,
                )
                .into_any_element(),
            })
            .child(
                h_flex().justify_end().child(
                    Button::new("finish")
                        .primary()
                        .label("Done")
                        .icon(I::Check)
                        .on_click(cx.listener(|this, _, _, cx| this.finish_setup(cx))),
                ),
            )
    }
}
