//! Quick support (`glass support`): someone who needs help runs one command, reads a short
//! code to the person helping them, and that person connects from any browser. Nothing
//! stays: the session has its own temporary config (token, keys, settings, logs), its host
//! runs only while `glass support` does, and stopping it deletes all of it.
//!
//! - The host is the ordinary one, started in the session's config dir with remote access
//!   on (WebRTC through the introducer, like `glass link --anywhere`) and asking before
//!   anyone connects. On macOS it runs as a short-lived launchd job, so macOS asks for
//!   Screen Recording and Accessibility for Glass itself, not for the Terminal it was typed
//!   in; the job is removed when the session ends.
//! - The code: the introducer (glassdesk.dev, or `GLASS_BASE`) keeps code → link for an
//!   hour, hashed and single-use; the helper types it at /help.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// How long the code (and the link behind it) waits for the helper.
pub const CODE_TTL: Duration = Duration::from_secs(3600);
/// How long the helper's access lasts once in (the session ends sooner when stopped).
const GRANT: Duration = Duration::from_secs(4 * 3600);
/// The host told to support a session (shows "You're helping …" in the viewer).
pub const HOST_ENV: &str = "GLASS_SUPPORT";
/// The `glass support` process the host belongs to; the host ends when it's gone.
const PARENT_ENV: &str = "GLASS_SUPPORT_PARENT";

/// This host is a quick-support session's.
pub fn is_host() -> bool {
    std::env::var_os(HOST_ENV).is_some()
}

/// A fresh, private config dir for one session; `glass support` points GLASS_CONFIG_DIR at
/// it before anything reads the config.
pub fn new_dir() -> Result<PathBuf> {
    let mut b = [0u8; 6];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).map_err(|_| anyhow::anyhow!("random"))?;
    let dir = std::env::temp_dir().join(format!("glass-support-{}", crate::remote::b64url(&b)));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(dir)
}

/// "123456789" → "123 456 789".
pub fn grouped(code: &str) -> String {
    let digits: Vec<char> = code.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.chunks(3).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
}

/// This computer's name, for the helper's screen ("You're helping Mum's MacBook").
pub fn computer_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output()
        && out.status.success()
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "this computer".into())
}

/// Where a session stands, for the window or the terminal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// The host is up.
    pub running: bool,
    /// Screen Recording and Accessibility (macOS asks the person to allow them once).
    pub screen: bool,
    pub input: bool,
    /// Someone asking to connect: (id, who).
    pub waiting: Vec<(u64, String)>,
    /// Who's connected now.
    pub viewers: Vec<String>,
}

enum Runner {
    Child(std::process::Child),
    #[cfg(target_os = "macos")]
    Launchd(String),
}

pub struct Session {
    dir: PathBuf,
    runner: Option<Runner>,
    /// The introducer the code is kept at.
    base: String,
    /// The helper's code, grouped ("123 456 789"); `None` when the introducer couldn't keep one.
    pub code: Option<String>,
    /// Revokes the code at the introducer.
    cancel: Option<String>,
    /// The link the code stands for (works on its own too).
    pub url: String,
    /// The host's web port (tests reach its /test hooks there).
    pub http_port: u16,
    /// Why there's no code, if there isn't.
    pub code_error: Option<String>,
}

impl Session {
    /// Starts the session's host in `dir` (the current GLASS_CONFIG_DIR), waits for it, and
    /// gets a code for it.
    pub fn start(dir: PathBuf, test: bool) -> Result<Self> {
        let settings = crate::settings::Settings {
            ask_everyone: true,
            name: Some(computer_name()),
            ..Default::default()
        };
        crate::settings::save(&settings)?;
        crate::remote::set_enabled(true)?;
        // Made here, before the host starts, so the two never race to create it.
        let key = crate::remote::HostKey::load_or_create()?;
        let http_port = free_port(false)?;
        let wt_port = free_port(true)?;
        let mut session = Self {
            dir: dir.clone(),
            runner: None,
            base: crate::remote::base().trim_end_matches('/').to_string(),
            code: None,
            cancel: None,
            url: String::new(),
            http_port,
            code_error: None,
        };
        session.runner = Some(spawn_host(&dir, http_port, wt_port, test)?);
        session.wait_for_host()?;
        session.url = new_link(&key)?;
        session.register();
        Ok(session)
    }

    fn wait_for_host(&mut self) -> Result<()> {
        for _ in 0..200 {
            if crate::status::read().is_some_and(|s| s.is_fresh()) && crate::ipc::call(&crate::ipc::Request::Viewers).is_ok() {
                return Ok(());
            }
            if let Some(Runner::Child(c)) = &mut self.runner
                && let Ok(Some(status)) = c.try_wait()
            {
                bail!("Glass stopped right away ({status}); see {}", self.dir.join("logs/host.log").display());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("Glass didn't start; see {}", self.dir.join("logs/host.log").display())
    }

    /// Asks the introducer for a code standing for the link. Without one (offline, or a
    /// self-hosted introducer without it), the link itself still works.
    fn register(&mut self) {
        match register(&self.base, &self.url) {
            Ok((code, cancel)) => {
                self.code = Some(grouped(&code));
                self.cancel = Some(cancel);
                self.code_error = None;
            }
            Err(e) => {
                self.code = None;
                self.cancel = None;
                self.code_error = Some(format!("{e:#}"));
            }
        }
    }

    /// A new code (and link), say after the first was used or mistyped too often.
    pub fn new_code(&mut self) -> Result<()> {
        self.revoke_code();
        self.url = new_link(&crate::remote::HostKey::load_or_create()?)?;
        self.register();
        Ok(())
    }

    fn revoke_code(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = agent().delete(format!("{}/api/support", self.base)).header("x-cancel", &cancel).call();
        }
    }

    /// Where things stand (quick: the status file and the host's control socket).
    pub fn state(&self) -> State {
        let status = crate::status::read().filter(|s| s.is_fresh());
        let mut state = State {
            running: status.is_some(),
            screen: status.as_ref().is_some_and(|s| s.screen_recording || s.capturing),
            input: status.as_ref().is_some_and(|s| s.accessibility),
            ..Default::default()
        };
        if let Ok(v) = crate::ipc::call(&crate::ipc::Request::Pending) {
            state.waiting = list(&v["pending"]).map(|p| (p["id"].as_u64().unwrap_or(0), text(&p["who"]))).collect();
        }
        if let Ok(v) = crate::ipc::call(&crate::ipc::Request::Viewers) {
            state.viewers = list(&v["viewers"]).map(|v| text(&v["describe"])).collect();
        }
        state
    }

    /// Lets someone in, or not.
    pub fn decide(&self, id: u64, allow: bool) -> Result<()> {
        crate::ipc::call(&crate::ipc::Request::Decide { id, allow })?;
        Ok(())
    }

    /// Ends the helper's session (they can ask again with a new code).
    pub fn end_sessions(&self) -> Result<()> {
        crate::ipc::call(&crate::ipc::Request::Disconnect { id: None })?;
        Ok(())
    }

    /// Ends everything: the code, the host, and the session's files.
    pub fn stop(&mut self) {
        self.revoke_code();
        match self.runner.take() {
            Some(Runner::Child(mut c)) => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(c.id() as i32, libc::SIGTERM);
                }
                for _ in 0..30 {
                    if matches!(c.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = c.kill();
                let _ = c.wait();
            }
            #[cfg(target_os = "macos")]
            Some(Runner::Launchd(label)) => {
                let _ = std::process::Command::new("launchctl")
                    .args(["bootout", &format!("gui/{}/{label}", unsafe { libc::getuid() })])
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            None => {}
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn list(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn free_port(udp: bool) -> Result<u16> {
    Ok(if udp {
        std::net::UdpSocket::bind(("0.0.0.0", 0))?.local_addr()?.port()
    } else {
        std::net::TcpListener::bind(("0.0.0.0", 0))?.local_addr()?.port()
    })
}

/// A link for the helper: works once, within the hour, from anywhere; the person is asked
/// before it lets anyone in.
fn new_link(key: &crate::remote::HostKey) -> Result<String> {
    let scope = crate::access::Scope { ask: true, ..Default::default() };
    let link = crate::access::create_link_for(&crate::access::file_for(&crate::default_token_file()), CODE_TTL, GRANT, scope)?;
    Ok(crate::remote::link_url(key, &link.code))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().new_agent()
}

/// Asks the introducer to keep `url` under a fresh code: (code, cancel token).
fn register(base: &str, url: &str) -> Result<(String, String)> {
    let mut reply = agent()
        .post(format!("{base}/api/support"))
        .send_json(json!({ "url": url }))
        .context("reach the support server")?;
    let v: Value = reply.body_mut().read_json()?;
    let code = v["code"].as_str().context("no code")?.to_string();
    let cancel = v["cancel"].as_str().context("no cancel token")?.to_string();
    Ok((code, cancel))
}

/// Starts the host for the session: a launchd job on macOS (so it, not Terminal, holds the
/// permissions), a child process elsewhere and in tests.
fn spawn_host(dir: &Path, http_port: u16, wt_port: u16, test: bool) -> Result<Runner> {
    let exe = std::env::current_exe()?;
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs)?;
    let mut args: Vec<String> = vec![
        "--http-port".into(),
        http_port.to_string(),
        "--https-port".into(),
        "0".into(),
        "--wt-port".into(),
        wt_port.to_string(),
        "--log-dir".into(),
        logs.display().to_string(),
    ];
    if test {
        args.push("--test".into());
    }
    let mut env: Vec<(String, String)> = vec![
        ("GLASS_CONFIG_DIR".into(), dir.display().to_string()),
        ("GLASS_NO_MENUBAR".into(), "1".into()),
        (HOST_ENV.into(), "1".into()),
        (PARENT_ENV.into(), std::process::id().to_string()),
    ];
    for k in ["GLASS_BASE", "GLASS_TEST_OS", "HOME", "PATH"] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.into(), v));
        }
    }
    #[cfg(target_os = "macos")]
    if !test {
        let label = format!("dev.glassdesk.support.{}", std::process::id());
        let plist = dir.join("job.plist");
        std::fs::write(&plist, launchd_plist(&label, &exe, &args, &env, &logs))?;
        let status = std::process::Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{}", unsafe { libc::getuid() })])
            .arg(&plist)
            .status()?;
        anyhow::ensure!(status.success(), "launchctl couldn't start Glass for this session");
        return Ok(Runner::Launchd(label));
    }
    let child = std::process::Command::new(exe)
        .args(&args)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(Runner::Child(child))
}

#[cfg(target_os = "macos")]
fn launchd_plist(label: &str, exe: &Path, args: &[String], env: &[(String, String)], logs: &Path) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let program: String = std::iter::once(exe.display().to_string())
        .chain(args.iter().cloned())
        .map(|a| format!("<string>{}</string>", esc(&a)))
        .collect();
    let vars: String = env.iter().map(|(k, v)| format!("<key>{}</key><string>{}</string>", esc(k), esc(v))).collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array>{program}</array>
<key>EnvironmentVariables</key><dict>{vars}</dict>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><false/>
<key>ProcessType</key><string>Interactive</string>
<key>StandardOutPath</key><string>{out}</string>
<key>StandardErrorPath</key><string>{out}</string>
</dict></plist>
"#,
        out = esc(&logs.join("stdout.log").display().to_string()),
    )
}

/// In the host: end when the `glass support` that started it is gone (killed, closed
/// Terminal), and take the session's files with it.
pub fn watch_parent() {
    let Some(pid) = std::env::var(PARENT_ENV).ok().and_then(|p| p.parse::<i32>().ok()) else { return };
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if !crate::platform::process_alive(pid as u32) {
                tracing::info!("support: glass support ended; stopping");
                let _ = std::fs::remove_dir_all(crate::config_dir());
                std::process::exit(0);
            }
        }
    });
}

// ---- The terminal ----

/// `glass support` without a window: the code, then who's asking (answered here) until
/// Ctrl+C. With `json`: one line {code, url, http_port, dir} and no questions (scripts, tests).
pub async fn run_terminal(mut session: Session, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            json!({ "code": session.code, "code_error": session.code_error, "url": session.url, "http_port": session.http_port, "dir": session.dir })
        );
    } else {
        print_code(&session);
    }
    let (tx, mut answers) = tokio::sync::mpsc::unbounded_channel::<String>();
    if !json {
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            while stdin.read_line(&mut line).is_ok_and(|n| n > 0) {
                let _ = tx.send(line.trim().to_lowercase());
                line.clear();
            }
        });
    }
    let mut asked: Option<u64> = None;
    let mut seen: Vec<String> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let stop = shutdown_signal();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tick.tick() => {
                let state = tokio::task::block_in_place(|| session.state());
                if json {
                    continue;
                }
                if !state.running {
                    eprintln!("\nGlass stopped unexpectedly. Run glass support again.");
                    break;
                }
                if asked.is_none()
                    && let Some((id, who)) = state.waiting.first()
                {
                    println!("\n{who} wants to see and control this computer. Allow? [y/n]");
                    asked = Some(*id);
                }
                if state.viewers != seen {
                    if state.viewers.is_empty() && !seen.is_empty() {
                        println!("Your helper left. Press Ctrl+C to stop sharing, or wait for them to come back.");
                    }
                    for v in state.viewers.iter().filter(|v| !seen.contains(*v)) {
                        println!("Connected: {v}. Press Ctrl+C to end the session.");
                    }
                    seen = state.viewers.clone();
                }
            }
            Some(answer) = answers.recv() => {
                if let Some(id) = asked.take() {
                    let allow = answer.starts_with('y');
                    let _ = session.decide(id, allow);
                    println!("{}", if allow { "Allowed." } else { "Turned away." });
                } else if answer == "n" || answer == "new" {
                    session.new_code()?;
                    print_code(&session);
                }
            }
        }
    }
    println!("\nStopping. Nothing from this session stays on this computer.");
    tokio::task::block_in_place(|| session.stop());
    Ok(())
}

fn print_code(session: &Session) {
    let helper = format!("{}/help", session.base);
    match &session.code {
        Some(code) => {
            println!("\n  Your code:  {code}\n");
            println!("Read it to the person helping you. They enter it at {helper}.");
            println!("Or send them this link: {}", session.url);
        }
        None => {
            println!("\nSend this link to the person helping you:\n\n  {}\n", session.url);
        }
    }
    println!("Waiting for your helper… (you'll be asked before they can see anything; Ctrl+C stops sharing)");
}

/// Ctrl+C, or the terminal closing (SIGTERM / SIGHUP).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        let mut hup = signal(SignalKind::hangup()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_read_in_threes() {
        assert_eq!(grouped("123456789"), "123 456 789");
        assert_eq!(grouped("123-456 789"), "123 456 789");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_job_is_a_plain_plist() {
        let p = launchd_plist(
            "dev.glassdesk.support.1",
            Path::new("/tmp/a b/glass"),
            &["--http-port".into(), "1".into()],
            &[("GLASS_CONFIG_DIR".into(), "/tmp/x&y".into())],
            Path::new("/tmp/logs"),
        );
        assert!(p.contains("<string>/tmp/a b/glass</string><string>--http-port</string>"));
        assert!(p.contains("<key>GLASS_CONFIG_DIR</key><string>/tmp/x&amp;y</string>"));
        assert!(p.contains("<key>KeepAlive</key><false/>"));
    }
}
