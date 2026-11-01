//! Links: make a one-time link with limits (how long, what it may do, from anywhere),
//! show it as a QR code, and see or cut off the devices let in by links.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable, ActiveTheme, Sizable, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::data;
use super::ui::{self, I};
use super::{GlassApp, act, actions};
use crate::access::Scope;

const GRANTS: [(&str, u64); 5] = [("30 min", 1800), ("1 hour", 3600), ("1 day", 86_400), ("1 week", 604_800), ("30 days", 2_592_000)];

pub struct LinkForm {
    grant: usize,
    control: bool,
    files: bool,
    clipboard: bool,
    ask: bool,
    passkey: bool,
    anywhere: bool,
    busy: bool,
    /// The last link made: (url, what it allows).
    made: Option<(String, String)>,
}

impl Default for LinkForm {
    fn default() -> Self {
        Self { grant: 2, control: true, files: true, clipboard: true, ask: false, passkey: false, anywhere: false, busy: false, made: None }
    }
}

impl GlassApp {
    pub(super) fn links(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.store.read(cx).snap.clone();
        let theme = cx.theme().clone();
        let f = &self.link;

        let chips = h_flex().gap_1().p_0p5().rounded(px(9.)).bg(theme.secondary_hover).children(GRANTS.iter().enumerate().map(|(i, (label, _))| {
            let on = f.grant == i;
            div()
                .id(("grant", i))
                .px_3()
                .py_1()
                .rounded(px(7.))
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(on, |d| d.bg(theme.background).shadow_sm())
                .when(!on, |d| d.text_color(theme.muted_foreground))
                .child(*label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.link.grant = i;
                    cx.notify();
                }))
        }));

        let switch = |id: &'static str, on: bool, set: fn(&mut LinkForm, bool), cx: &mut Context<Self>| {
            Switch::new(id).checked(on).on_click(cx.listener(move |this, on: &bool, _, cx| {
                set(&mut this.link, *on);
                cx.notify();
            }))
        };
        let form = vec![
            ui::row("Access lasts", "From when the link is opened.", chips, cx),
            ui::row("Mouse and keyboard", "Off: they can only watch.", switch("l-control", f.control, |f, v| f.control = v, cx), cx),
            ui::row("Files", "Send files here and download from here.", switch("l-files", f.files, |f, v| f.files = v, cx), cx),
            ui::row("Clipboard", "Copy and paste both ways.", switch("l-clip", f.clipboard, |f, v| f.clipboard = v, cx), cx),
            ui::row("Ask me each time", format!("You allow each connection, here or from the {}.", super::ICON_PLACE), switch("l-ask", f.ask, |f, v| f.ask = v, cx), cx),
            ui::row("Require a passkey", "Face ID, Touch ID or a security key, on every connection.", switch("l-passkey", f.passkey, |f, v| f.passkey = v, cx), cx),
            ui::row(
                "Works from anywhere",
                "Over the internet, end-to-end encrypted. Off: only from your tailnet.",
                switch("l-anywhere", f.anywhere, |f, v| f.anywhere = v, cx),
                cx,
            ),
            h_flex()
                .px_4()
                .py_3()
                .justify_end()
                .child(
                    Button::new("make")
                        .primary()
                        .icon(I::Link)
                        .label("Make link")
                        .loading(f.busy)
                        .disabled(!snap.running() || !snap.policy.allows("links"))
                        .on_click(cx.listener(|this, _, window, cx| this.make_link(window, cx))),
                )
                .into_any_element(),
        ];

        let made = f.made.clone().map(|(url, summary)| {
            let copy = url.clone();
            h_flex()
                .p_5()
                .gap_5()
                .rounded(px(16.))
                .border_1()
                .border_color(theme.primary.opacity(0.4))
                .bg(theme.primary.opacity(0.06))
                .child(ui::qr(&url, 148.))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("Your link is ready"))
                        .child(div().text_sm().text_color(theme.muted_foreground).child(summary))
                        .child(ui::mono(url).p_2().rounded(px(7.)).bg(theme.background).border_1().border_color(theme.border).truncate())
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Button::new("copy-link").small().icon(I::Copy).label("Copy").on_click(move |_, window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    super::toast(window, cx, Ok("Link copied.".into()));
                                }))
                                .child(Button::new("done-link").small().ghost().label("Done").on_click(cx.listener(|this, _, _, cx| {
                                    this.link.made = None;
                                    cx.notify();
                                }))),
                        ),
                )
        });

        let mut devices: Vec<AnyElement> = snap
            .passes
            .iter()
            .map(|p| {
                let id = p.id.clone();
                ui::icon_row(
                    if p.passkey { I::KeyRound } else { I::MonitorSmartphone },
                    theme.primary,
                    p.device.clone(),
                    format!("{} · {}", p.scope, data::until(p.expires)),
                    Button::new(SharedString::from(format!("revoke-{id}"))).ghost().small().label("Revoke").on_click(move |_, window, cx| {
                        let id = id.clone();
                        act(window, cx, move || actions::revoke(Some(&id)))
                    }),
                    cx,
                )
            })
            .collect();
        let any = !devices.is_empty();
        if !any {
            devices.push(ui::note("No devices have link access right now.", cx));
        }

        ui::page("Links", "One-time links let a device in without your token, with limits you choose.", cx)
            .children(made)
            .child(ui::section("New link", form, cx))
            .child(ui::section("Devices with access", devices, cx).child(
                h_flex().justify_end().child(
                    Button::new("revoke-all")
                        .danger()
                        .small()
                        .label("Revoke every link")
                        .on_click(|_, window, cx| act(window, cx, || actions::revoke(None))),
                ),
            ))
            .when(!any, |d| d)
    }

    fn make_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let f = &self.link;
        let grant = Duration::from_secs(GRANTS[f.grant].1);
        let scope = Scope { view_only: !f.control, files: f.files, clipboard: f.clipboard, ask: f.ask, passkey: f.passkey };
        let anywhere = f.anywhere;
        self.link.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_executor().spawn(async move { actions::link(grant, anywhere, scope) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.link.busy = false;
                match result {
                    Ok(made) => this.link.made = Some(made),
                    Err(e) => super::toast(window, cx, Err(e)),
                }
                super::Store::refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }
}
