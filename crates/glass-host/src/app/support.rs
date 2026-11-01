//! The quick-support window (`glass support` on the Mac's own screen): the code to read to
//! the helper, the two permissions macOS asks for, who's asking to connect (allow or not),
//! who's connected, and Stop sharing. Closing it ends the session and leaves nothing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Icon, Sizable, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use super::ui::{self, I};
use crate::platform::permissions;
use crate::support::{Session, State};

type Shared = Arc<Mutex<Session>>;

pub fn run(rt: tokio::runtime::Runtime, session: Session) -> anyhow::Result<()> {
    let session: Shared = Arc::new(Mutex::new(session));
    // Ctrl+C in the Terminal it was started from (or the Terminal closing) ends it too.
    let on_signal = session.clone();
    rt.spawn(async move {
        crate::support::shutdown_signal().await;
        on_signal.lock().unwrap().stop();
        std::process::exit(0);
    });
    std::mem::forget(rt);

    let on_quit = session.clone();
    gpui_kit::application().with_assets(ui::Assets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.activate(true);
        let stop = on_quit.clone();
        cx.on_window_closed(move |cx, _| {
            if cx.windows().is_empty() {
                stop.lock().unwrap().stop();
                cx.quit();
            }
        })
        .detach();
        cx.bind_keys([KeyBinding::new("cmd-q", super::Quit, None)]);
        let stop = on_quit.clone();
        cx.on_action(move |_: &super::Quit, cx| {
            stop.lock().unwrap().stop();
            cx.quit();
        });
        cx.set_menus([Menu { name: "Glass".into(), items: vec![MenuItem::action("Stop sharing and quit", super::Quit)], disabled: false }]);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(480.), px(640.)), cx)),
            window_min_size: Some(size(px(420.), px(560.))),
            window_background: WindowBackgroundAppearance::Blurred,
            app_id: Some("dev.glassdesk.support".into()),
            ..TitleBar::window_options()
        };
        let session = on_quit.clone();
        gpui_kit::open_window(options, cx, |window, cx| {
            super::apply_theme(window, cx);
            cx.new(|cx| SupportView::new(session, cx))
        })
        .expect("open the support window");
    });
    Ok(())
}

struct SupportView {
    session: Shared,
    state: State,
    code: Option<String>,
    url: String,
    host: String,
}

impl SupportView {
    fn new(session: Shared, cx: &mut Context<Self>) -> Self {
        let (code, url) = {
            let s = session.lock().unwrap();
            (s.code.clone(), s.url.clone())
        };
        let host = crate::remote::base().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string();
        // Who's asking and who's in, twice a second, read off the main thread.
        let poll = session.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let s = poll.clone();
                let state = cx.background_executor().spawn(async move { s.lock().unwrap().state() }).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    if this.state != state {
                        this.state = state;
                        cx.notify();
                    }
                }) else {
                    break;
                };
                cx.background_executor().timer(Duration::from_millis(500)).await;
            }
        })
        .detach();
        // macOS shows its own prompts the first time; these rows are for after.
        Self { session, state: State::default(), code, url, host }
    }

    fn act(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut Session) + Send + 'static) {
        let s = self.session.clone();
        cx.background_executor().spawn(async move { f(&mut s.lock().unwrap()) }).detach();
    }

    fn code_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let copy = self.code.clone().unwrap_or_else(|| self.url.clone());
        v_flex()
            .items_center()
            .gap_3()
            .py_6()
            .px_5()
            .rounded(px(18.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(theme.muted_foreground).child(if self.code.is_some() {
                "YOUR CODE"
            } else {
                "YOUR LINK"
            }))
            .child(match &self.code {
                Some(code) => div()
                    .font_family("Menlo")
                    .text_size(px(40.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(code.clone())
                    .into_any_element(),
                None => ui::mono(self.url.clone()).text_color(theme.foreground).into_any_element(),
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("copy").small().ghost().icon(I::Copy).label("Copy").on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                        super::toast(window, cx, Ok("Copied.".into()));
                    }))
                    .child(Button::new("new-code").small().ghost().icon(I::RefreshCw).label("New code").on_click(cx.listener(|this, _, window, cx| {
                        let s = this.session.clone();
                        cx.spawn_in(window, async move |this, cx| {
                            let r = cx
                                .background_executor()
                                .spawn(async move {
                                    let mut s = s.lock().unwrap();
                                    s.new_code().map(|_| (s.code.clone(), s.url.clone()))
                                })
                                .await;
                            let _ = this.update_in(cx, |this, window, cx| match r {
                                Ok((code, url)) => {
                                    this.code = code;
                                    this.url = url;
                                    cx.notify();
                                }
                                Err(e) => super::toast(window, cx, Err(e)),
                            });
                        })
                        .detach();
                    }))),
            )
    }

    fn status(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let state = &self.state;
        let mut rows: Vec<AnyElement> = Vec::new();
        if !state.running {
            return ui::group(vec![ui::note("Starting…", cx)], cx).into_any_element();
        }
        if !state.screen {
            rows.push(ui::icon_row(
                I::Monitor,
                theme.warning,
                "Allow Screen Recording",
                "So your helper can see this screen. Switch on \"glass\" in the list, then come back.",
                Button::new("allow-screen").small().primary().label("Allow…").on_click(|_, _, _| permissions::open_pane(permissions::SCREEN_RECORDING_PANE)),
                cx,
            ));
        }
        if !state.input {
            rows.push(ui::icon_row(
                I::MousePointerClick,
                theme.warning,
                "Allow Accessibility",
                "So your helper can use the mouse and keyboard. Switch on \"glass\".",
                Button::new("allow-input").small().primary().label("Allow…").on_click(|_, _, _| permissions::open_pane(permissions::ACCESSIBILITY_PANE)),
                cx,
            ));
        }
        for (id, who) in state.waiting.clone() {
            rows.push(ui::icon_row(
                I::Hand,
                theme.primary,
                "Someone wants to connect",
                who,
                h_flex()
                    .gap_2()
                    .child(Button::new(("deny", id)).small().ghost().label("Don't allow").on_click(cx.listener(move |this, _, _, cx| {
                        this.act(cx, move |s| {
                            let _ = s.decide(id, false);
                        })
                    })))
                    .child(Button::new(("allow", id)).small().primary().label("Allow").on_click(cx.listener(move |this, _, _, cx| {
                        this.act(cx, move |s| {
                            let _ = s.decide(id, true);
                        })
                    }))),
                cx,
            ));
        }
        for (i, who) in state.viewers.clone().into_iter().enumerate() {
            rows.push(ui::icon_row(
                I::MonitorSmartphone,
                theme.success,
                "Helping you now",
                who,
                Button::new(("end", i)).small().ghost().label("End").on_click(cx.listener(|this, _, _, cx| {
                    this.act(cx, |s| {
                        let _ = s.end_sessions();
                    })
                })),
                cx,
            ));
        }
        if state.waiting.is_empty() && state.viewers.is_empty() {
            rows.push(
                h_flex()
                    .px_4()
                    .py_3p5()
                    .gap_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(I::LoaderCircle).small())
                    .child(div().flex_1().min_w_0().child("Waiting for your helper… You'll be asked before they can see anything."))
                    .into_any_element(),
            );
        }
        ui::group(rows, cx).into_any_element()
    }
}

impl Render for SupportView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let helping = !self.state.viewers.is_empty();
        v_flex()
            .size_full()
            .bg(theme.background.opacity(0.92))
            .child(TitleBar::new())
            .child(
                v_flex()
                    .flex_1()
                    .px_8()
                    .pb_6()
                    .gap_5()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(ui::logo(40.))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(if helping {
                                        "You're being helped"
                                    } else {
                                        "Get help with this computer"
                                    }))
                                    .child(div().text_sm().text_color(theme.muted_foreground).child(format!(
                                        "Read the code to the person helping you. They type it at {}/help.",
                                        self.host
                                    ))),
                            ),
                    )
                    .child(self.code_card(cx))
                    .child(self.status(cx))
                    .child(div().flex_1())
                    .child(
                        v_flex()
                            .gap_2()
                            .child(Button::new("stop").danger().w_full().icon(I::Power).label("Stop sharing").on_click({
                                // Stops the session itself: a dispatched Quit reached no handler on
                                // Windows (closing the window worked; this button did nothing).
                                let session = self.session.clone();
                                move |_, _, cx| {
                                    session.lock().unwrap().stop();
                                    cx.quit();
                                }
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_center()
                                    .text_color(theme.muted_foreground)
                                    .child("Stopping ends the session and removes everything it used."),
                            ),
                    ),
            )
    }
}
