//! The app's building blocks: page headers, grouped rows like System Settings, status
//! dots, the logo and QR codes, and the icons it embeds.

use std::borrow::Cow;
use std::sync::Arc;

use gpui_kit::component::{ActiveTheme, Icon, Sizable, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

pub use gpui_kit::assets::IconName as I;

gpui_kit::assets::icon_assets!(
    Extra,
    [
        House, MonitorSmartphone, Link, Network, ClockFading, Bot, Settings, Building2, Info, Copy, ExternalLink,
        Check, CircleCheck, CircleAlert, TriangleAlert, Power, RefreshCw, Download, Shield, ShieldCheck, Eye,
        EyeOff, QrCode, Globe, Lock, KeyRound, Plus, Trash, X, Pause, Play, Square, Send, Folder, Sparkles,
        RotateCw, SquareTerminal, MessageSquare, FingerprintPattern, Hand, Users, Zap, Clipboard, FileText,
        ChevronRight, LogOut, CircleX, MousePointerClick, LoaderCircle, RadioTower, Monitor, Laptop, Server,
        Mail, Ban, Video, Palette, Wifi, Moon, CircleDot, ArrowRight, Bell, Smartphone, Mic
    ]
);

/// Tailscale's mark: nine dots, the top row dim, the middle solid, the bottom dim, solid, dim.
pub fn tailscale() -> Icon {
    const SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g fill="#000">
<circle cx="5" cy="5" r="2.6" fill-opacity=".38"/><circle cx="12" cy="5" r="2.6" fill-opacity=".38"/><circle cx="19" cy="5" r="2.6" fill-opacity=".38"/>
<circle cx="5" cy="12" r="2.6"/><circle cx="12" cy="12" r="2.6"/><circle cx="19" cy="12" r="2.6"/>
<circle cx="5" cy="19" r="2.6" fill-opacity=".38"/><circle cx="12" cy="19" r="2.6"/><circle cx="19" cy="19" r="2.6" fill-opacity=".38"/></g></svg>"##;
    Icon::default().data(SVG)
}

/// The component icons, plus the ones above.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = Extra.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(Extra.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

static LOGO: &[u8] = include_bytes!("../../assets/icon.png");

pub fn logo(side: f32) -> impl IntoElement {
    thread_local! {
        static IMAGE: Arc<Image> = Arc::new(Image::from_bytes(ImageFormat::Png, LOGO.to_vec()));
    }
    img(ImageSource::Image(IMAGE.with(Arc::clone))).size(px(side)).flex_none()
}

pub fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(7.)).flex_none().rounded_full().bg(color)
}

pub fn count(n: usize, bg: Hsla, fg: Hsla) -> impl IntoElement {
    div()
        .min_w(px(18.))
        .h(px(18.))
        .px_1p5()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .child(n.to_string())
}

/// A page: a title, a line saying what it's for, then its sections.
pub fn page(title: &str, subtitle: &str, cx: &App) -> Div {
    v_flex().w_full().max_w(px(760.)).px(px(36.)).pt_1().pb(px(40.)).gap(px(26.)).child(
        v_flex()
            .gap_1()
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(subtitle.to_string())),
    )
}

/// A titled group of rows.
pub fn section(title: &str, rows: Vec<AnyElement>, cx: &App) -> Div {
    v_flex()
        .gap_2()
        .when(!title.is_empty(), |d| {
            d.child(
                div()
                    .px_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(title.to_uppercase()),
            )
        })
        .child(group(rows, cx))
}

/// Rows in one rounded box, with hairlines between them.
pub fn group(rows: Vec<AnyElement>, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .rounded(px(12.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .overflow_hidden()
        .children(rows.into_iter().enumerate().map(|(i, row)| div().when(i > 0, |d| d.border_t_1().border_color(theme.border)).child(row)))
}

/// A setting's detail, saying so when a policy manages it (its control is then disabled).
pub fn managed(detail: impl Into<String>, locked: bool) -> String {
    let detail = detail.into();
    if locked { format!("{detail} Managed by your organization.") } else { detail }
}

/// A row: what it is, a line of detail, and its control on the right.
pub fn row(title: impl Into<SharedString>, detail: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> AnyElement {
    let detail: SharedString = detail.into();
    h_flex()
        .min_h(px(52.))
        .px_4()
        .py_2p5()
        .gap_4()
        .justify_between()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title.into()))
                .when(!detail.is_empty(), |d| d.child(div().text_xs().text_color(cx.theme().muted_foreground).child(detail))),
        )
        .child(h_flex().flex_none().gap_2().child(control))
        .into_any_element()
}

/// A row led by a tinted icon.
pub fn icon_row(
    icon: I,
    tint: Hsla,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> AnyElement {
    let detail: SharedString = detail.into();
    h_flex()
        .min_h(px(56.))
        .px_4()
        .py_2p5()
        .gap_3()
        .child(tile(icon, tint))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(title.into()))
                .when(!detail.is_empty(), |d| {
                    d.child(div().text_xs().text_color(cx.theme().muted_foreground).truncate().child(detail))
                }),
        )
        .child(h_flex().flex_none().gap_2().child(control))
        .into_any_element()
}

/// A rounded square with an icon, tinted.
pub fn tile(icon: I, tint: Hsla) -> impl IntoElement {
    div()
        .size(px(30.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .bg(tint.opacity(0.14))
        .child(Icon::new(icon).small().text_color(tint))
}

/// A line of quiet text in a group (an empty list, a hint).
pub fn note(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    div().px_4().py_3p5().text_sm().text_color(cx.theme().muted_foreground).child(text.into()).into_any_element()
}

pub fn mono(text: impl Into<SharedString>) -> Div {
    div().font_family("Menlo").text_xs().child(text.into())
}

/// A QR code on a white card, drawn module by module.
pub fn qr(text: &str, side: f32) -> impl IntoElement {
    let modules = qrcode::QrCode::new(text.as_bytes()).ok().map(|code| {
        let n = code.width();
        let dark: Vec<bool> = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
        (n, dark)
    });
    div().size(px(side)).flex_none().rounded(px(14.)).bg(white()).p(px(12.)).child(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let Some((n, dark)) = &modules else { return };
                // Whole pixels per module, centred, so there are no seams.
                let cell = (f32::from(bounds.size.width) / *n as f32).floor().max(1.);
                let offset = (f32::from(bounds.size.width) - cell * *n as f32) / 2.;
                let ink: Hsla = rgb(0x0b0b0d).into();
                for (i, on) in dark.iter().enumerate() {
                    if *on {
                        let (x, y) = ((i % n) as f32, (i / n) as f32);
                        let origin = point(bounds.origin.x + px(offset + x * cell), bounds.origin.y + px(offset + y * cell));
                        window.paint_quad(fill(Bounds::new(origin, size(px(cell), px(cell))), ink));
                    }
                }
            },
        )
        .size_full(),
    )
}

/// A small status label: a dot and a word.
pub fn status(text: impl Into<SharedString>, color: Hsla) -> impl IntoElement {
    h_flex().gap_1p5().text_xs().font_weight(FontWeight::MEDIUM).text_color(color).child(dot(color)).child(text.into())
}
