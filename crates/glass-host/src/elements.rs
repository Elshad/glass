//! Agents reading apps as text instead of pixels (`snapshot`), and acting on what they
//! read (`press`, `set_value`) through Accessibility, without moving the person's pointer
//! or typing. Each element an agent can act on gets a ref like `@k3f9:e12`, good only for
//! that snapshot: acting on one first checks the element is still there and still the
//! same, and refuses rather than act on whatever took its place. macOS (Accessibility)
//! and Windows (UI Automation).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::computer::Refused;

/// An app to read.
pub struct Found {
    pub pid: i32,
    pub bundle: String,
    pub name: String,
}

/// One element of an app's window.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
pub struct Node {
    /// Short and lowercase: "button", "textfield", "text".
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    /// A password field: its value is never read.
    pub secret: bool,
    pub enabled: bool,
    pub focused: bool,
    pub editable: bool,
    /// What's under it wasn't read (too deep, too many, too slow).
    pub cut: bool,
    /// Its place in `Walk::handles`, when it can be acted on.
    pub handle: Option<usize>,
    pub children: Vec<Node>,
}

/// A walk of an app: its windows (and open menus), and the elements that can be acted on.
pub struct Walk {
    pub roots: Vec<Node>,
    pub handles: Vec<ax::Element>,
    pub partial: bool,
}

/// Limits on one walk.
pub struct Budget {
    pub depth: usize,
    pub nodes: usize,
    pub until: Instant,
}

/// What a kept element is now.
pub struct Now {
    pub role: String,
    pub name: Option<String>,
    pub enabled: bool,
    /// Where it is: x, y, width, height in global points.
    pub frame: Option<[f64; 4]>,
}

/// What became of an action, as far as the app says.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
pub enum Outcome {
    Done,
    /// Sent, but the app didn't reply in time: it may or may not have happened.
    Unconfirmed,
    /// The element can't do that through Accessibility: nothing happened.
    Unsupported,
    /// The element is gone: nothing happened.
    Gone,
    Failed(String),
}

#[cfg(target_os = "macos")]
use crate::platform::ax;
#[cfg(windows)]
use crate::platform::uia as ax;

#[cfg(not(any(target_os = "macos", windows)))]
mod ax {
    use super::{Budget, Found, Now, Outcome, Walk};

    const NOT_HERE: &str = "Reading apps as text works on macOS and Windows only for now; use screenshot.";

    pub struct Element;

    pub fn find_app(_: Option<&str>) -> Result<Found, String> {
        Err(NOT_HERE.into())
    }
    pub fn walk(_: &Found, _: bool, _: Budget) -> Result<Walk, String> {
        Err(NOT_HERE.into())
    }
    pub fn look(_: &Element) -> Option<Now> {
        None
    }
    pub fn press(_: &Element) -> Outcome {
        Outcome::Failed(NOT_HERE.into())
    }
    pub fn set_value(_: &Element, _: &str) -> Outcome {
        Outcome::Failed(NOT_HERE.into())
    }
    pub fn value(_: &Element) -> Option<String> {
        None
    }
}

/// What the OS calls the interface apps describe themselves through.
const VIA: &str = if cfg!(windows) { "UI Automation" } else { "Accessibility" };

/// Lines a snapshot shows unless the agent asks for more, and at most.
const LINES: usize = 400;
const MOST_LINES: usize = 2000;

/// A walk stops after this many elements, this deep (not counting empty wrappers), or
/// after this long, and says what it skipped.
const MOST_NODES: usize = 6000;
const MOST_DEPTH: usize = 40;
const WALK_TIME: Duration = Duration::from_secs(4);

/// Snapshots kept for each agent (refs from older ones are refused), and in all.
const KEPT_EACH: usize = 3;
const KEPT_ALL: usize = 16;

/// An element a snapshot gave a ref, and what it was then.
struct Kept {
    handle: ax::Element,
    role: String,
    name: Option<String>,
    secret: bool,
    editable: bool,
}

struct Snapshot {
    id: String,
    asker: String,
    pid: i32,
    app: String,
    kept: Vec<Arc<Kept>>,
}

static SNAPSHOTS: Mutex<Vec<Snapshot>> = Mutex::new(Vec::new());

fn new_id() -> String {
    const DIGITS: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut b = [0u8; 4];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).expect("random");
    b.iter().map(|x| DIGITS[*x as usize % DIGITS.len()] as char).collect()
}

fn keep(snapshot: Snapshot) {
    let mut all = SNAPSHOTS.lock().unwrap();
    let asker = snapshot.asker.clone();
    all.push(snapshot);
    while all.iter().filter(|s| s.asker == asker).count() > KEPT_EACH {
        let oldest = all.iter().position(|s| s.asker == asker).unwrap();
        all.remove(oldest);
    }
    while all.len() > KEPT_ALL {
        all.remove(0);
    }
}

/// `@k3f9:e12` (or without the @) as the snapshot id and element number.
fn parse_ref(r: &str) -> Option<(&str, usize)> {
    let (snap, el) = r.trim().trim_start_matches('@').split_once(':')?;
    let n = el.strip_prefix('e')?.parse().ok()?;
    (!snap.is_empty() && n > 0).then_some((snap, n))
}

/// The element a ref names, if it's from one of this agent's recent snapshots.
fn resolve(r: &str) -> Result<(i32, String, Arc<Kept>), Refused> {
    let Some((snap, n)) = parse_ref(r) else {
        return Err(Refused(format!("{r:?} isn't a ref: use one from snapshot, like @k3f9:e12.")));
    };
    let asker = crate::computer::asker();
    let all = SNAPSHOTS.lock().unwrap();
    let s = all
        .iter()
        .find(|s| s.id == snap && s.asker == asker)
        .ok_or_else(|| Refused(format!("Snapshot {snap} is gone (only your last {KEPT_EACH} are kept). Take a new snapshot. Nothing was done.")))?;
    let kept = s.kept.get(n - 1).cloned().ok_or_else(|| Refused(format!("Snapshot {snap} has no element e{n}. Nothing was done.")))?;
    Ok((s.pid, s.app.clone(), kept))
}

/// Text that fits on a line.
fn clip(s: &str, most: usize) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > most { format!("{}…", s.chars().take(most).collect::<String>()) } else { s }
}

fn label(role: &str, name: Option<&str>) -> String {
    match name {
        Some(n) => format!("{role} {:?}", clip(n, 60)),
        None => format!("the {role}"),
    }
}

/// Which nodes to show, at what depth, in order. A node shows if it can be acted on, is
/// text, is a window or menu, or was cut short; an unnamed one that doesn't is left out
/// (its children take its place), and a named one shows only if something under it does.
fn lines<'a>(n: &'a Node, depth: usize, root: bool, out: &mut Vec<(usize, &'a Node)>) {
    let text = matches!(n.role.as_str(), "text" | "heading") && n.name.is_some();
    let show = root || n.handle.is_some() || text || n.cut;
    let start = out.len();
    if show {
        out.push((depth, n));
    }
    for c in &n.children {
        lines(c, if show { depth + 1 } else { depth }, false, out);
    }
    if !show && n.name.is_some() && out.len() > start {
        out.insert(start, (depth, n));
        for l in &mut out[start + 1..] {
            l.0 += 1;
        }
    }
}

fn line(depth: usize, n: &Node, r: Option<&str>) -> String {
    let mut s = format!("{}{}", "  ".repeat(depth), n.role);
    if let Some(name) = &n.name {
        s += &format!(" {:?}", clip(name, 100));
    }
    if n.secret {
        s += " (password, hidden)";
    } else if let Some(v) = &n.value {
        match (n.role.as_str(), v.as_str()) {
            ("checkbox" | "radio" | "switch" | "tab", "1") => s += " checked",
            ("checkbox" | "radio" | "switch" | "tab", "0") => {}
            ("checkbox", "2") => s += " mixed",
            _ => s += &format!(" = {:?}", clip(v, 80)),
        }
    }
    if !n.enabled {
        s += " disabled";
    }
    if n.focused {
        s += " focused";
    }
    if n.cut {
        s += " …";
    }
    if let Some(r) = r {
        s += &format!(" {r}");
    }
    s
}

/// A walk as text, the nodes that got refs (e1 first), and how many lines didn't fit.
fn render<'a>(id: &str, roots: &'a [Node], most: usize) -> (String, Vec<&'a Node>, usize) {
    let mut all = Vec::new();
    for r in roots {
        lines(r, 0, true, &mut all);
    }
    let mut out = String::new();
    let mut refs = Vec::new();
    for (depth, n) in all.iter().take(most) {
        let r = n.handle.map(|_| {
            refs.push(*n);
            format!("@{id}:e{}", refs.len())
        });
        out += &line(*depth, n, r.as_deref());
        out.push('\n');
    }
    (out, refs, all.len().saturating_sub(most))
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R, Refused> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Refused(e.to_string()))
}

/// An agent under a grant doesn't read protected apps, or apps outside its limit.
fn readable(app: &Found) -> Result<(), Refused> {
    let crate::grants::Actor::Agent(g) = crate::grants::current() else { return Ok(()) };
    if crate::grants::PROTECTED_APPS.contains(&app.bundle.as_str()) {
        return Err(Refused(format!("{} is protected: agents don't read it. Ask the person with request_help.", app.name)));
    }
    if let Some(apps) = &g.apps
        && !apps.contains(&app.bundle)
    {
        return Err(Refused(format!("This agent only sees {}.", apps.join(", "))));
    }
    Ok(())
}

pub async fn snapshot(app: Option<String>, all_windows: bool, most: Option<usize>) -> Result<Value, Refused> {
    crate::computer::may_act(true).await?;
    let found = blocking(move || ax::find_app(app.as_deref())).await?.map_err(Refused)?;
    readable(&found)?;
    let (found, walk) = blocking(move || {
        let budget = Budget { depth: MOST_DEPTH, nodes: MOST_NODES, until: Instant::now() + WALK_TIME };
        let walk = ax::walk(&found, all_windows, budget);
        (found, walk)
    })
    .await?;
    let walk = walk.map_err(Refused)?;
    let id = new_id();
    let (tree, refs, more) = render(&id, &walk.roots, most.unwrap_or(LINES).clamp(20, MOST_LINES));

    // Keep what got a ref; let the rest go.
    let mut handles: Vec<Option<ax::Element>> = walk.handles.into_iter().map(Some).collect();
    let mut kept = Vec::new();
    for n in &refs {
        let Some(handle) = n.handle.and_then(|h| handles.get_mut(h)?.take()) else { continue };
        kept.push(Arc::new(Kept { handle, role: n.role.clone(), name: n.name.clone(), secret: n.secret, editable: n.editable }));
    }
    let count = kept.len();
    keep(Snapshot { id: id.clone(), asker: crate::computer::asker(), pid: found.pid, app: found.name.clone(), kept });

    let mut head = format!("{} ({}), snapshot {id}: {count} elements to act on with press or set_value, by ref.", found.name, found.bundle);
    if walk.partial {
        head += " Parts marked … weren't read (too large or slow): use screenshot for those.";
    }
    if more > 0 {
        head += &format!(" {more} more lines not shown (pass max_lines for more).");
    }
    crate::computer::announce("snapshot", format!("of {}", found.name), None);
    Ok(json!({ "text": format!("{head}\n\n{tree}"), "snapshot": id, "refs": count }))
}

/// The element a ref names, checked to be still there and still the same: what it says
/// now (for labels), and where it is.
async fn current(r: &str) -> Result<(i32, Arc<Kept>, String, Option<[f64; 4]>), Refused> {
    let (pid, app, kept) = resolve(r)?;
    let k = kept.clone();
    let now = blocking(move || ax::look(&k.handle)).await?;
    let gone = || Refused(format!("{} in {app} is gone (the window changed). Take a new snapshot. Nothing was done.", label(&kept.role, kept.name.as_deref())));
    let Now { role, name, enabled, frame } = now.ok_or_else(gone)?;
    // A field's name can come from its placeholder, which goes once there's text.
    if role != kept.role || (!kept.editable && name != kept.name) {
        return Err(Refused(format!(
            "{r} was {} but is now {}: the window changed. Take a new snapshot. Nothing was done.",
            label(&kept.role, kept.name.as_deref()),
            label(&role, name.as_deref())
        )));
    }
    let what = label(&kept.role, kept.name.as_deref());
    if !enabled {
        return Err(Refused(format!("{what} is disabled right now. Nothing was done.")));
    }
    crate::grants::check_input(crate::grants::Target::App(pid)).map_err(Refused)?;
    Ok((pid, kept, what, frame))
}

fn center(frame: Option<[f64; 4]>) -> Option<(f64, f64)> {
    frame.map(|[x, y, w, h]| (x + w / 2.0, y + h / 2.0))
}

/// Where to click instead, in the agent's screenshot pixels.
async fn click_instead(frame: Option<[f64; 4]>) -> String {
    match center(frame) {
        Some((x, y)) => match crate::computer::locate(x, y).await {
            Some((d, px, py, ..)) => format!(" Click it instead, at {px:.0}, {py:.0} on display {d}."),
            None => " Click it instead (take a screenshot to find it).".into(),
        },
        None => " Click it instead (take a screenshot to find it).".into(),
    }
}

async fn show_at(kind: &str, detail: String, frame: Option<[f64; 4]>) {
    let at = match center(frame) {
        Some((x, y)) => crate::computer::locate(x, y).await.map(|(d, _, _, nx, ny)| (d, nx, ny)),
        None => None,
    };
    crate::computer::announce(kind, detail, at);
}

pub async fn press(r: &str) -> Result<Value, Refused> {
    crate::computer::may_act(false).await?;
    let (_, kept, what, frame) = current(r).await?;
    let k = kept.clone();
    let outcome = blocking(move || ax::press(&k.handle)).await?;
    let said = match outcome {
        Outcome::Done => format!("Pressed {what}."),
        Outcome::Unconfirmed => format!(
            "Pressed {what}, but the app didn't confirm it in time (it may be busy, or a dialog opened). Check with snapshot or screenshot before pressing again."
        ),
        Outcome::Unsupported => {
            return Err(Refused(format!("{what} can't be pressed through {VIA}; nothing was done.{}", click_instead(frame).await)));
        }
        Outcome::Gone => return Err(Refused(format!("{what} is gone (the window changed). Take a new snapshot. Nothing was done."))),
        Outcome::Failed(why) => {
            return Err(Refused(format!("Pressing {what} failed ({why}); it may not have happened. Check before trying again.")));
        }
    };
    show_at("press", what, frame).await;
    Ok(json!({ "text": said }))
}

pub async fn set_value(r: &str, text: &str) -> Result<Value, Refused> {
    crate::computer::may_act(false).await?;
    if text.chars().count() > 10_000 {
        return Err(Refused("too much text at once (10,000 characters at most)".into()));
    }
    let (_, kept, what, frame) = current(r).await?;
    if !kept.editable {
        return Err(Refused(format!("{what} isn't a text field: use press, or click it and type_text. Nothing was done.")));
    }
    if kept.secret && matches!(crate::grants::current(), crate::grants::Actor::Agent(_)) {
        return Err(Refused(format!("{what} is a password field. Ask the person to fill it in with request_help.")));
    }
    let (k, t) = (kept.clone(), text.to_string());
    let (outcome, now) = blocking(move || {
        let outcome = ax::set_value(&k.handle, &t);
        let now = if k.secret { None } else { ax::value(&k.handle) };
        (outcome, now)
    })
    .await?;
    let said = match outcome {
        Outcome::Done | Outcome::Unconfirmed => match now {
            _ if kept.secret => format!("Filled in {what}."),
            Some(v) if v == text => format!("Set {what}."),
            Some(v) => format!("Set {what}, but it now shows {:?}: the app may have changed or refused it.", clip(&v, 80)),
            None => format!("Set {what}, but couldn't read it back to check."),
        },
        Outcome::Unsupported => {
            return Err(Refused(format!("{what} can't be filled in through {VIA}; nothing was done. Click it and use type_text.")));
        }
        Outcome::Gone => return Err(Refused(format!("{what} is gone (the window changed). Take a new snapshot. Nothing was done."))),
        Outcome::Failed(why) => return Err(Refused(format!("Filling in {what} failed ({why}). Check it before trying again."))),
    };
    // What was set isn't shown to viewers or logged: it may be a secret.
    show_at("fill", format!("{what} ({} characters)", text.chars().count()), frame).await;
    Ok(json!({ "text": said }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, name: Option<&str>, handle: Option<usize>, children: Vec<Node>) -> Node {
        Node {
            role: role.into(),
            name: name.map(Into::into),
            value: None,
            secret: false,
            enabled: true,
            focused: false,
            editable: false,
            cut: false,
            handle,
            children,
        }
    }

    #[test]
    fn refs_parse_only_whole() {
        assert_eq!(parse_ref("@k3f9:e12"), Some(("k3f9", 12)));
        assert_eq!(parse_ref("k3f9:e1"), Some(("k3f9", 1)));
        assert_eq!(parse_ref("@e12"), None);
        assert_eq!(parse_ref("@k3f9:e0"), None);
        assert_eq!(parse_ref("@:e3"), None);
    }

    #[test]
    fn wrappers_fold_away_and_named_groups_stay_when_they_hold_something() {
        let tree = node("window", Some("Doc"), None, vec![
            node("group", None, None, vec![node("group", None, None, vec![node("button", Some("Save"), Some(0), vec![])])]),
            node("group", Some("Sidebar"), None, vec![node("link", Some("Home"), Some(1), vec![])]),
            node("group", Some("Empty"), None, vec![node("image", None, None, vec![])]),
            node("text", Some("Hello"), None, vec![]),
        ]);
        let roots = [tree];
        let (text, refs, more) = render("ab12", &roots, 100);
        assert_eq!(
            text,
            "window \"Doc\"\n  button \"Save\" @ab12:e1\n  group \"Sidebar\"\n    link \"Home\" @ab12:e2\n  text \"Hello\"\n"
        );
        assert_eq!((refs.iter().map(|n| n.handle).collect::<Vec<_>>(), more), (vec![Some(0), Some(1)], 0));
    }

    #[test]
    fn a_long_tree_stops_at_the_limit_and_says_so() {
        let kids = (0..10).map(|i| node("button", Some(&format!("B{i}")), Some(i), vec![])).collect();
        let roots = [node("window", None, None, kids)];
        let (text, refs, more) = render("ab12", &roots, 4);
        assert_eq!(text.lines().count(), 4);
        assert_eq!(refs.len(), 3);
        assert_eq!(more, 7);
    }

    #[test]
    fn passwords_never_show_and_states_read_plainly() {
        let mut pw = node("passwordfield", Some("Password"), Some(0), vec![]);
        pw.secret = true;
        pw.value = Some("hunter2".into());
        assert_eq!(line(0, &pw, Some("@x:e1")), "passwordfield \"Password\" (password, hidden) @x:e1");
        let mut cb = node("checkbox", Some("Remember me"), Some(1), vec![]);
        cb.value = Some("1".into());
        cb.enabled = false;
        assert_eq!(line(1, &cb, None), "  checkbox \"Remember me\" checked disabled");
    }

    #[test]
    fn only_the_last_few_snapshots_of_each_agent_are_kept() {
        for i in 0..5 {
            keep(Snapshot { id: format!("t{i}"), asker: "test-keeper".into(), pid: 1, app: "X".into(), kept: Vec::new() });
        }
        let ids: Vec<String> = SNAPSHOTS.lock().unwrap().iter().filter(|s| s.asker == "test-keeper").map(|s| s.id.clone()).collect();
        assert_eq!(ids, ["t2", "t3", "t4"]);
    }
}

