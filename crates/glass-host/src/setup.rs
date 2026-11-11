//! `glass setup`: an interactive walkthrough from fresh install to a working connection.

use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use cliclack::{confirm, intro, log, note, outro, spinner};
use console::style;

use crate::status::{self, Status};
use crate::{access, agent, platform, tailscale};

use platform::setup::{TAILSCALE_INSTALL, TAILSCALE_SIGN_IN, TAILSCALE_START};
const TAILSCALE_DNS_ADMIN: &str = "https://login.tailscale.com/admin/dns";
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a device let in by setup's link keeps access: a week, as it's the owner's iPad.
const ANYWHERE_GRANT: Duration = Duration::from_secs(7 * 24 * 3600);

pub async fn run() -> Result<()> {
    intro(style(" glass ").black().on_cyan())?;
    log::remark("Remote control for this computer from any browser, with very low latency.")?;

    let tailnet = tailscale_step().await?;
    platform::setup::prepare().await?;
    if tailnet.is_some() {
        portless_step().await?;
    }
    service_step()?;
    let status = wait_for_host()?;
    platform::setup::permissions(status)?;
    finish(tailnet)
}

/// Returns this Mac's tailnet name when remote access is set up.
async fn tailscale_step() -> Result<Option<String>> {
    log::step("Remote access with Tailscale")?;
    loop {
        match tailscale::probe().await {
            tailscale::Probe::Missing => {
                log::warning(format!(
                    "Tailscale isn't installed. It's free and gives your devices a private, secure link.\n\
                     {TAILSCALE_INSTALL}"
                ))?;
                if !ask_retry(
                    "Installed it?",
                    "Skip (connect with a one-time link instead)",
                )? {
                    return Ok(None);
                }
            }
            tailscale::Probe::NotRunning => {
                log::warning(TAILSCALE_START)?;
                if !ask_retry("Started it?", "Skip remote access for now")? {
                    return Ok(None);
                }
            }
            tailscale::Probe::LoggedOut => {
                log::warning(TAILSCALE_SIGN_IN)?;
                if !ask_retry("Signed in?", "Skip remote access for now")? {
                    return Ok(None);
                }
            }
            tailscale::Probe::NoHttps { name } => {
                log::warning(format!(
                    "Browsers need HTTPS for Glass. Turn on {} for your tailnet\n\
                     (one switch at the bottom of {TAILSCALE_DNS_ADMIN}), so {name} can get a certificate.",
                    style("HTTPS Certificates").bold()
                ))?;
                platform::setup::open_url(TAILSCALE_DNS_ADMIN);
                if !ask_retry("Turned it on?", "Skip remote access for now")? {
                    return Ok(None);
                }
            }
            tailscale::Probe::Ready { name } => {
                let sp = spinner();
                sp.start(format!("Getting a certificate for {name}"));
                match tailscale::ensure_cert(&crate::config_dir().join("certs")).await {
                    Some(_) => {
                        sp.stop(format!("Tailscale ready: {}", style(&name).cyan()));
                        return Ok(Some(name));
                    }
                    None => {
                        sp.error("Couldn't get a certificate from Tailscale");
                        // On Linux, non-root users need to be Tailscale "operators" to fetch certs.
                        #[cfg(target_os = "linux")]
                        if confirm("Allow your user to manage Tailscale certificates? (runs `sudo tailscale set --operator=$USER`)")
                            .initial_value(true)
                            .interact()?
                        {
                            let user = std::env::var("USER").unwrap_or_default();
                            let _ = std::process::Command::new("sudo")
                                .args(["tailscale", "set", &format!("--operator={user}")])
                                .status();
                            continue;
                        }
                        if !ask_retry("Try again?", "Skip remote access for now")? {
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }
}

fn service_step() -> Result<()> {
    log::step("Background service")?;
    // Installed for everyone (the MSI): it starts at sign-in already, for every user.
    #[cfg(windows)]
    if agent::machine_dir().is_some() {
        agent::install(true)?;
        log::success("Glass runs in the background and starts when anyone signs in (installed for everyone)")?;
        return Ok(());
    }
    let at_login = confirm("Start Glass automatically when you log in?")
        .initial_value(true)
        .interact()?;
    let sp = spinner();
    sp.start("Installing the Glass service");
    agent::install(at_login)?;
    sp.stop(if at_login {
        "Glass runs in the background and starts at login"
    } else {
        "Glass is running in the background"
    });
    Ok(())
}

fn wait_for_host() -> Result<Status> {
    let sp = spinner();
    sp.start("Starting Glass");
    let started = Instant::now();
    loop {
        if let Some(s) = status::read().filter(|s| s.is_fresh()) {
            sp.stop(format!("Glass {} is running", s.version));
            return Ok(s);
        }
        if started.elapsed() > START_TIMEOUT {
            sp.error("Glass didn't start");
            anyhow::bail!("{}", agent::LOG_HINT.trim());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Linux can't listen on 443 without root, so Tailscale forwards it to Glass instead and
/// links need no port. (macOS listens on 443 itself.) Optional: :8080 works regardless.
#[cfg(target_os = "linux")]
async fn portless_step() -> Result<()> {
    if tailscale::serves_https_to(8080).await {
        return Ok(());
    }
    match tailscale::serve_https_to(8080).await {
        Ok(()) => log::success("Links work without a port number (tailscale serve)")?,
        Err(e) => log::remark(format!("Links will include :8080 ({e:#})"))?,
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn portless_step() -> Result<()> {
    Ok(())
}

/// The address the running host reports (port-less when it has 443), else the :8080 one.
fn web_url(tailnet_name: &str) -> String {
    crate::status::read()
        .filter(|s| s.is_fresh())
        .and_then(|s| s.url)
        .filter(|u| u.contains(tailnet_name))
        .unwrap_or_else(|| format!("https://{tailnet_name}:8080/"))
}

fn finish(tailnet: Option<String>) -> Result<()> {
    let token = std::fs::read_to_string(crate::default_token_file()).unwrap_or_default();
    let token = token.trim();
    match tailnet {
        Some(name) => {
            let url = web_url(&name);
            note(
                "Connect from your iPad",
                format!(
                    "{qr}\n\
                     Scan this, or open {url} on any device signed in to your Tailscale.\n\
                     Tip: Share › Add to Home Screen for a true fullscreen app.\n\n\
                     Devices outside your tailnet need this token: {token}",
                    qr = qr(&url),
                    url = style(&url).cyan().underlined(),
                ),
            )?;
        }
        // No tailnet: a one-time link still connects any browser, directly over WebRTC.
        None => {
            log::step("Connect from anywhere")?;
            let make = confirm(
                "Make a link for your iPad? It works from any network, end-to-end encrypted, with no open ports.",
            )
            .initial_value(true)
            .interact()?;
            let link = if make {
                let base = status::read().and_then(|s| s.url).unwrap_or_default();
                Some(crate::make_link(&base, ANYWHERE_GRANT, true, crate::access::Scope::default())?)
            } else {
                None
            };
            match link {
                Some((url, link)) => note(
                    "Connect from your iPad",
                    format!(
                        "{qr}\n\
                         Scan this, or open the link on the iPad:\n{url}\n\n\
                         Works on one device, within {ttl}, and gives that device {grant}.\n\
                         Make another any time: {again}",
                        qr = qr(&url),
                        url = style(&url).cyan().underlined(),
                        ttl = access::describe(access::LINK_TTL),
                        grant = access::describe(link.grant),
                        again = style("glass link --anywhere").bold(),
                    ),
                )?,
                None => note(
                    "Use Glass on this computer",
                    format!(
                        "Open {} to try it here.\n\
                         To connect another device later, run {}, or set up Tailscale and run {} again.",
                        style("http://localhost:8080/").cyan(),
                        style("glass link --anywhere").bold(),
                        style("glass setup").bold(),
                    ),
                )?,
            }
        }
    }
    outro(format!(
        "All set. Problems or ideas? {}",
        style("glass feedback \"…\"").bold()
    ))?;
    Ok(())
}

/// Confirm with a retry/skip choice. Returns true to retry.
pub(crate) fn ask_retry(retry: &str, skip: &str) -> io::Result<bool> {
    cliclack::select(retry)
        .item(true, "Yes, check again", "")
        .item(false, skip, "")
        .interact()
}

pub(crate) fn qr(text: &str) -> String {
    use qrcode::render::unicode::Dense1x2;
    match qrcode::QrCode::new(text) {
        Ok(code) => code.render::<Dense1x2>().quiet_zone(true).build(),
        Err(_) => String::new(),
    }
}

// ---- Unattended (`glass setup --yes`) ----

/// A step that still needs a person (or an agent with more access).
#[derive(Debug, serde::Serialize)]
pub struct Need {
    pub step: &'static str,
    /// What to do, in one sentence a person can follow.
    pub action: String,
    /// True if it must happen at the machine itself (a click on its screen).
    pub on_screen: bool,
}

#[derive(serde::Serialize)]
struct Report {
    ok: bool,
    version: &'static str,
    os: String,
    url: Option<String>,
    tailnet_name: Option<String>,
    running: bool,
    capturing: bool,
    input: bool,
    needs: Vec<Need>,
}

/// Does everything that needs no human, then reports what's left.
pub async fn run_unattended(json: bool) -> Result<()> {
    let mut needs = Vec::new();

    let tailnet = match tailscale::probe().await {
        tailscale::Probe::Ready { name } => {
            match tailscale::ensure_cert(&crate::config_dir().join("certs")).await {
                Some(_) => Some(name),
                None if platform::setup::try_tailscale_operator()
                    && tailscale::ensure_cert(&crate::config_dir().join("certs"))
                        .await
                        .is_some() =>
                {
                    Some(name)
                }
                None => {
                    needs.push(Need {
                    step: "tailscale-cert",
                    action: "Allow this user to fetch Tailscale certificates: sudo tailscale set --operator=$USER".into(),
                    on_screen: false,
                });
                    None
                }
            }
        }
        tailscale::Probe::NoHttps { .. } => {
            needs.push(Need {
                step: "tailscale-https",
                action: format!(
                    "Enable HTTPS Certificates for the tailnet at {TAILSCALE_DNS_ADMIN}"
                ),
                on_screen: false,
            });
            None
        }
        tailscale::Probe::NotRunning => {
            needs.push(Need {
                step: "tailscale-start",
                action: TAILSCALE_START.replace('\n', " "),
                on_screen: cfg!(target_os = "macos"),
            });
            None
        }
        tailscale::Probe::LoggedOut => {
            needs.push(Need {
                step: "tailscale-login",
                action: TAILSCALE_SIGN_IN.into(),
                on_screen: cfg!(target_os = "macos"),
            });
            None
        }
        tailscale::Probe::Missing => {
            needs.push(Need {
                step: "tailscale-install",
                action: format!(
                    "{} Or skip Tailscale: `glass link --anywhere` makes a link that connects from any network.",
                    TAILSCALE_INSTALL.replace('\n', " ")
                ),
                on_screen: false,
            });
            None
        }
    };

    needs.extend(platform::setup::prepare_unattended());
    if tailnet.is_some() {
        #[cfg(target_os = "linux")]
        if !tailscale::serves_https_to(8080).await {
            // Optional; without it links just include :8080.
            let _ = tailscale::serve_https_to(8080).await;
        }
    }
    let service = agent::install(true);
    if let Err(e) = &service {
        needs.push(Need {
            step: "service",
            action: format!("Couldn't install the background service: {e:#}"),
            on_screen: false,
        });
    }

    // Give the service a moment to start and report.
    let started = Instant::now();
    let mut status = None;
    while service.is_ok() && started.elapsed() < START_TIMEOUT {
        status = status::read().filter(|s| s.is_fresh());
        if status.as_ref().is_some_and(|s| s.capturing) {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let running = status.is_some();
    let s = status.unwrap_or_default();
    if running {
        needs.extend(platform::setup::permission_needs(&s));
    } else if service.is_ok() {
        needs.push(Need {
            step: "service",
            action: format!("Glass didn't start;{}", agent::LOG_HINT),
            on_screen: false,
        });
    }

    // Platform checks before and after starting can flag the same step.
    let mut seen = std::collections::HashSet::new();
    needs.retain(|n| seen.insert(n.step));

    let report = Report {
        ok: needs.is_empty(),
        version: env!("CARGO_PKG_VERSION"),
        os: platform::os_version(),
        url: tailnet.as_deref().map(web_url).or(s.url.clone()),
        tailnet_name: tailnet,
        running,
        capturing: s.capturing,
        input: s.accessibility,
        needs,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Glass {} {}",
            report.version,
            if report.ok {
                "is ready"
            } else {
                "is installed; still needed:"
            }
        );
        for n in &report.needs {
            println!(
                "  - [{}{}] {}",
                n.step,
                if n.on_screen {
                    ", on this machine's screen"
                } else {
                    ""
                },
                n.action
            );
        }
        if let Some(url) = &report.url {
            println!("Open: {url}");
        }
    }
    Ok(())
}
