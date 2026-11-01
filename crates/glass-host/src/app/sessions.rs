//! Connected: who's viewing now (end any of them), who's waiting to be let in, a message
//! to everyone, and the privacy screen.

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::ui::{self, I};
use super::{GlassApp, act, actions};

impl GlassApp {
    pub(super) fn sessions(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.store.read(cx).snap.clone();
        let theme = cx.theme().clone();

        let mut waiting: Vec<AnyElement> = snap
            .pending
            .iter()
            .map(|p| {
                let id = p.id;
                ui::icon_row(
                    I::Hand,
                    theme.warning,
                    p.who.clone(),
                    "Waiting for you to answer",
                    h_flex()
                        .gap_2()
                        .child(Button::new(("deny", id)).ghost().small().label("Deny").on_click(move |_, window, cx| {
                            act(window, cx, move || actions::decide(id, false))
                        }))
                        .child(Button::new(("allow", id)).primary().small().label("Allow").on_click(move |_, window, cx| {
                            act(window, cx, move || actions::decide(id, true))
                        })),
                    cx,
                )
            })
            .collect();
        if waiting.is_empty() {
            waiting.push(ui::note("Nobody is waiting. Turn on asking in Settings to approve each connection.", cx));
        }

        let mut viewing: Vec<AnyElement> = snap
            .viewers
            .iter()
            .map(|v| {
                let id = v.id;
                ui::icon_row(
                    I::MonitorSmartphone,
                    theme.primary,
                    v.describe.clone(),
                    "",
                    Button::new(("end", id)).ghost().small().label("End").on_click(move |_, window, cx| {
                        act(window, cx, move || actions::kick(Some(id)))
                    }),
                    cx,
                )
            })
            .collect();
        if viewing.is_empty() {
            viewing.push(ui::note("Nobody is connected right now.", cx));
        }

        let live = !snap.viewers.is_empty();
        if snap.chat.len() != self.chat_seen {
            self.chat_seen = snap.chat.len();
            self.chat_scroll.scroll_to_bottom();
        }
        let conversation = if snap.chat.is_empty() {
            ui::note(if live { "No messages yet. Viewers can write from the chat button in their toolbar." } else { "No messages yet." }, cx)
        } else {
            div()
                .id("chat-log")
                .max_h(px(320.))
                .overflow_y_scroll()
                .track_scroll(&self.chat_scroll)
                .child(v_flex().px_4().py_3().gap_2().children(snap.chat.iter().map(|m| {
                    let bubble = div()
                        .max_w(relative(0.78))
                        .px_3()
                        .py_1p5()
                        .rounded(px(12.))
                        .text_sm()
                        .child(m.text.clone());
                    let bubble = if m.mine {
                        bubble.bg(theme.primary).text_color(theme.primary_foreground)
                    } else {
                        bubble.bg(theme.secondary_hover)
                    };
                    v_flex()
                        .gap_0p5()
                        .map(|d| if m.mine { d.items_end() } else { d.items_start() })
                        .child(
                            div()
                                .px_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{} · {}", if m.mine { "You" } else { m.from.as_str() }, super::data::when(m.at))),
                        )
                        .child(bubble)
                })))
                .into_any_element()
        };
        let tools = vec![
            conversation,
            h_flex()
                .px_4()
                .py_3()
                .gap_2()
                .child(div().flex_1().child(Input::new(&self.chat).small().disabled(!live)))
                .child(
                    Button::new("send")
                        .primary()
                        .small()
                        .icon(I::Send)
                        .label("Send")
                        .disabled(!live)
                        .on_click(cx.listener(|this, _, window, cx| this.send_chat(window, cx))),
                )
                .into_any_element(),
            ui::row(
                "Privacy screen",
                ui::managed("Blank this screen and block its keyboard and mouse until everyone leaves.", snap.policy.locked("privacy")),
                Switch::new("privacy-now").checked(snap.privacy).disabled(!live || snap.policy.locked("privacy")).on_click(|on: &bool, window, cx| {
                    let on = *on;
                    act(window, cx, move || actions::privacy(on))
                }),
                cx,
            ),
        ];

        ui::page("Connected", "Who's viewing this computer, and who's asking to.", cx)
            .when(!snap.pending.is_empty() || snap.settings.ask_guests || snap.settings.ask_everyone, |d| {
                d.child(ui::section("Waiting to connect", waiting, cx))
            })
            .child(
                ui::section("Connected now", viewing, cx).when(snap.viewers.len() > 1, |d| {
                    d.child(
                        h_flex().justify_end().child(
                            Button::new("end-all")
                                .danger()
                                .small()
                                .label("End every session")
                                .on_click(|_, window, cx| act(window, cx, || actions::kick(None))),
                        ),
                    )
                }),
            )
            .child(ui::section("Messages", tools, cx))
            .when(!snap.running(), |d| d.child(ui::note("Glass isn't running, so nobody can connect.", cx)))
    }
}
