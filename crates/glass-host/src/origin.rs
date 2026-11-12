//! Which browser requests Glass answers. Any web page can send Glass requests and open
//! WebTransport sessions with it, and Glass trusts some callers for where they are (the
//! owner's own tailnet devices), not for anything a page would have to know. So a page on
//! another site, open on one of the owner's devices, could drive this computer; and a page
//! whose own domain is made to resolve to this computer (DNS rebinding) even counts as
//! Glass's own page, so it could read the answers too.
//!
//! So Glass only answers a `Host` that can't be rebound: an IP address, localhost, a name
//! only the local network or the tailnet resolves, this computer's own names, or one
//! `glass config allowed-hosts` names. And it turns away what a browser sends from another
//! site's page (its `Origin`) unless `glass config allowed-origins` names that site.
//! Programs that aren't browsers send no `Origin` and get through.
//!
//! Adapted from voxl (MIT, github.com/tembo/voxl).

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tracing::warn;

/// How long `allowed-hosts` and `allowed-origins` are kept before being read again.
const SETTINGS_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
struct Rules {
    /// This computer's own names: its tailnet name, its certificate's, `--wt-host`.
    names: Vec<String>,
    /// Its tailnet's domain (`tail1234.ts.net`), which only that tailnet's machines are on.
    tailnets: Vec<String>,
    /// `allowed-hosts`: ".example.com" also covers its subdomains, "*" any host.
    hosts: Vec<String>,
    /// `allowed-origins` (`scheme://host[:port]`), "*" any origin.
    origins: Vec<String>,
}

impl Rules {
    fn new(names: &[String], hosts: &[String], origins: &[String]) -> Self {
        let clean = |v: &String| v.trim().trim_end_matches('/').to_ascii_lowercase();
        let names: Vec<String> = names.iter().map(|n| split_host(n).0).filter(|n| !n.is_empty()).collect();
        let mut tailnets: Vec<String> = Vec::new();
        for n in &names {
            if let Some((_, domain)) = n.split_once('.')
                && domain.ends_with(".ts.net")
                && !tailnets.iter().any(|t| t == domain)
            {
                tailnets.push(domain.to_string());
            }
        }
        Rules {
            names,
            tailnets,
            // A port given with a host name doesn't matter: the name is what's rebound.
            hosts: hosts.iter().map(|h| split_host(&clean(h)).0).filter(|h| !h.is_empty()).collect(),
            origins: origins.iter().map(|o| origin_key(&clean(o))).collect(),
        }
    }

    fn host_allowed(&self, name: &str) -> bool {
        name.parse::<IpAddr>().is_ok()
            || local_name(name)
            || self.names.iter().any(|n| n == name)
            || self.tailnets.iter().any(|t| in_domain(name, t))
            || self.hosts.iter().any(|h| {
                h == "*" || h == name || h.strip_prefix('.').is_some_and(|domain| name == domain || in_domain(name, domain))
            })
    }

    fn origin_listed(&self, origin: &str) -> bool {
        let key = origin_key(origin);
        self.origins.iter().any(|o| o == "*" || *o == key)
    }

    /// Why an HTTP request for `path` with these `Host` and `Origin` headers is turned
    /// away, or `None` to answer it.
    fn refuse(&self, path: &str, host: Option<&str>, origin: Option<&str>) -> Option<String> {
        let (name, port) = host.map(split_host).unwrap_or_default();
        // Browsers always send a Host, so one without it comes from another kind of client.
        if host.is_some() && !self.host_allowed(&name) {
            return Some(format!(
                "Glass doesn't answer for the host {name:?}: a site could point that name at \
                 this computer (DNS rebinding). Open this computer at its tailnet name or IP \
                 address, or, to serve it under that name, run `glass config allowed-hosts {name}` on it."
            ));
        }
        let origin = origin?.trim();
        // Open to every site: OAuth metadata, registration and tokens take no cookies.
        if crate::oauth::any_origin(path) {
            return None;
        }
        let lower = origin.to_ascii_lowercase();
        if self.origin_listed(&lower) {
            return None;
        }
        let parts = split_origin(&lower);
        // Glass's own page: the origin the browser loaded it from is the host it asks for.
        let own = host.is_some()
            && parts.as_ref().is_some_and(|(n, p, default)| *n == name && p.unwrap_or(*default) == port.unwrap_or(*default));
        // MCP clients on this device's loopback (MCP Inspector) still need a token there.
        let local_mcp = path == "/mcp" && parts.as_ref().is_some_and(|(n, _, _)| loopback(n));
        (!own && !local_mcp).then(|| {
            format!(
                "Glass only takes requests from its own pages, not from {origin}. To let that \
                 site call this computer, run `glass config allowed-origins {origin}` on it."
            )
        })
    }

    /// Why a WebTransport session to `authority` from a page at `origin` is turned away.
    /// The viewer drives any Glass host on the tailnet, so a page on another of the
    /// owner's machines (or on localhost) may connect, not only this host's own page.
    fn refuse_session(&self, authority: &str, origin: Option<&str>) -> Option<String> {
        let (name, _) = split_host(authority);
        if !self.host_allowed(&name) {
            return Some(format!("the host {name:?} isn't one of this computer's names"));
        }
        let origin = origin?.trim();
        let lower = origin.to_ascii_lowercase();
        if self.origin_listed(&lower) {
            return None;
        }
        let ok = split_origin(&lower).is_some_and(|(n, _, _)| {
            n == name
                || loopback(&n)
                || self.names.contains(&n)
                || self.tailnets.iter().any(|t| in_domain(&n, t))
                || n.parse::<IpAddr>().is_ok_and(crate::tailscale::is_tailscale_ip)
                || self.hosts.iter().any(|h| h == &n || h.strip_prefix('.').is_some_and(|d| n == d || in_domain(&n, d)))
        });
        (!ok).then(|| format!("a page at {origin} isn't Glass's own"))
    }
}

/// Names only this computer, the local network or the tailnet resolve, which no site can
/// point elsewhere: single labels (MagicDNS short names, LAN host names), localhost, and
/// the special-use local domains.
fn local_name(name: &str) -> bool {
    (!name.is_empty() && !name.contains('.'))
        || [".localhost", ".local", ".lan", ".home.arpa", ".internal"].iter().any(|d| name.ends_with(d))
}

fn loopback(name: &str) -> bool {
    name == "localhost" || name.ends_with(".localhost") || name.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// `name` is a subdomain of `domain`.
fn in_domain(name: &str, domain: &str) -> bool {
    name.strip_suffix(domain).is_some_and(|rest| rest.len() > 1 && rest.ends_with('.'))
}

/// Splits a `Host` value into its lowercase name (an IPv6 address without the brackets)
/// and its port. A port that isn't a number becomes 0, which matches no origin.
fn split_host(v: &str) -> (String, Option<u16>) {
    let v = v.trim();
    let (name, port) = match v.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((ip, after)) => (ip, after.strip_prefix(':')),
            None => (rest, None),
        },
        None => match v.rsplit_once(':') {
            Some((n, p)) if !n.contains(':') => (n, Some(p)),
            _ => (v, None),
        },
    };
    (name.trim_end_matches('.').to_ascii_lowercase(), port.map(|p| p.parse().unwrap_or(0)))
}

/// Splits an origin into its host name, its port and its scheme's default port; `None`
/// for one that isn't http(s), such as the "null" of a sandboxed frame.
fn split_origin(v: &str) -> Option<(String, Option<u16>, u16)> {
    let (scheme, rest) = v.split_once("://")?;
    let default = match scheme {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => return None,
    };
    let (name, port) = split_host(rest);
    Some((name, port, default))
}

/// An origin with its port written out, so `https://a.example` and `https://a.example:443`
/// compare equal; anything else as it is.
fn origin_key(v: &str) -> String {
    match (v.split_once("://"), split_origin(v)) {
        (Some((scheme, _)), Some((name, port, default))) => format!("{scheme}://{name}:{}", port.unwrap_or(default)),
        _ => v.to_string(),
    }
}

// ---- What this computer is called ----

static NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CACHE: Mutex<Option<(Instant, Arc<Rules>)>> = Mutex::new(None);

fn learn(names: impl IntoIterator<Item = String>) {
    let mut known = NAMES.lock().unwrap();
    for n in names {
        let n = split_host(&n).0;
        if !n.is_empty() && !known.contains(&n) {
            known.push(n);
        }
    }
    drop(known);
    *CACHE.lock().unwrap() = None;
}

/// The name in a URL like `https://box.tail1.ts.net:8443/`.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    Some(split_host(rest.split(['/', '?', '#']).next()?).0)
}

/// Learns the names this computer is served under before the web server starts: the one
/// in its URL (its certificate's), `--wt-host`, and its tailnet names, which are looked up
/// again now and then (Tailscale may come up later, or the machine be renamed).
pub async fn start(wt_host: Option<&str>) {
    let url = crate::status::current().and_then(|s| s.url).and_then(|u| url_host(&u));
    learn(url.into_iter().chain(wt_host.map(String::from)));
    // Tests never ask Tailscale.
    if crate::testmode::enabled() {
        return;
    }
    let names = crate::tailscale::self_names().await;
    let mut found = !names.is_empty();
    learn(names);
    tokio::spawn(async move {
        let mut wait = Duration::from_secs(30);
        loop {
            tokio::time::sleep(if found { Duration::from_secs(600) } else { wait }).await;
            wait = (wait * 2).min(Duration::from_secs(600));
            let names = crate::tailscale::self_names().await;
            found = !names.is_empty();
            learn(names);
        }
    });
}

fn rules() -> Arc<Rules> {
    let mut cache = CACHE.lock().unwrap();
    if let Some((at, r)) = cache.as_ref()
        && at.elapsed() < SETTINGS_TTL
    {
        return r.clone();
    }
    let s = crate::settings::load();
    let r = Arc::new(Rules::new(&NAMES.lock().unwrap(), &s.allowed_hosts, &s.allowed_origins));
    *cache = Some((Instant::now(), r.clone()));
    r
}

fn value(req: &Request, name: HeaderName) -> Option<&str> {
    // A value that isn't text can't name an allowed host or origin.
    req.headers().get(name).map(|v| v.to_str().unwrap_or(""))
}

/// Middleware for every route: answers 403, saying why, instead of running the handler
/// for a request [`Rules`] turns away.
pub async fn check(req: Request, next: Next) -> Response {
    // HTTP/2 names the host in the URI (`:authority`), not a Host header.
    let host = value(&req, header::HOST).or_else(|| req.uri().authority().map(|a| a.as_str()));
    if let Some(why) = rules().refuse(req.uri().path(), host, value(&req, header::ORIGIN)) {
        warn!("refused {} {}: {why}", req.method(), req.uri().path());
        return (StatusCode::FORBIDDEN, why).into_response();
    }
    next.run(req).await
}

/// Why a WebTransport session request is turned away, or `None` to accept it.
pub fn refuse_session(authority: &str, origin: Option<&str>) -> Option<String> {
    rules().refuse_session(authority, origin)
}

/// A JSON body has to say it is one. A page on another site may POST `text/plain`
/// without asking first (a CORS "simple" request), never `application/json`.
pub fn require_json(headers: &HeaderMap) -> Option<Response> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"));
    (!json).then(|| {
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({ "error": "send the body as JSON, with Content-Type: application/json" })))
            .into_response()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glass() -> Rules {
        Rules::new(&["box.tail1.ts.net".into(), "localhost".into()], &[], &[])
    }

    fn served(r: &Rules, host: &str, origin: Option<&str>) -> bool {
        r.refuse("/agent/v1/act", Some(host), origin).is_none()
    }

    #[test]
    fn rebinding_host_names_are_refused() {
        let r = glass();
        // A site's own name, pointed at this computer.
        assert!(!served(&r, "attacker.example:8080", None));
        assert!(!served(&r, "attacker.example", Some("http://attacker.example")));
        // Another tailnet's name (anyone's Funnel), or one merely ending like ours.
        assert!(!served(&r, "box.tail2.ts.net", None));
        assert!(!served(&r, "eviltail1.ts.net", None));
        let why = r.refuse("/", Some("attacker.example"), None).unwrap();
        assert!(why.contains("attacker.example") && why.contains("allowed-hosts"), "{why}");
        // Names that can't be rebound, and this computer's own.
        for host in [
            "127.0.0.1:8080",
            "[::1]:8080",
            "100.64.0.2:8080",
            "[fd7a:115c:a1e0::2]:8080",
            "localhost:8080",
            "glass.localhost",
            "box:8080",
            "box.local:8080",
            "box.tail1.ts.net",
            "BOX.tail1.ts.net.:443",
            "studio.tail1.ts.net:8080",
        ] {
            assert!(served(&r, host, None), "{host}");
        }
        // Clients that send no Host at all (HTTP/1.0) aren't browsers.
        assert!(r.refuse("/", None, None).is_none());
    }

    #[test]
    fn other_sites_pages_are_refused() {
        let r = glass();
        assert!(!served(&r, "box.tail1.ts.net", Some("https://attacker.example")));
        assert!(!served(&r, "100.64.0.2:8080", Some("https://attacker.example")));
        assert!(!served(&r, "box.tail1.ts.net", Some("null")));
        // Another machine's page on the tailnet, or another app on this one, is another site.
        assert!(!served(&r, "box.tail1.ts.net", Some("https://studio.tail1.ts.net")));
        assert!(!served(&r, "localhost:8080", Some("http://localhost:3000")));
        // Same name, other port: another site.
        assert!(!served(&r, "box.tail1.ts.net", Some("https://box.tail1.ts.net:8443")));
        assert!(!served(&r, "box.tail1.ts.net:8080", Some("http://box.tail1.ts.net")));
        let why = r.refuse("/agent/v1/act", Some("box.tail1.ts.net"), Some("https://attacker.example")).unwrap();
        assert!(why.contains("https://attacker.example") && why.contains("allowed-origins"), "{why}");
    }

    #[test]
    fn glass_own_pages_and_programs_are_served() {
        let r = glass();
        assert!(served(&r, "box.tail1.ts.net", Some("https://box.tail1.ts.net")));
        assert!(served(&r, "box.tail1.ts.net:443", Some("https://box.tail1.ts.net")));
        assert!(served(&r, "box.tail1.ts.net:8080", Some("https://box.tail1.ts.net:8080")));
        assert!(served(&r, "localhost:8080", Some("http://localhost:8080")));
        assert!(served(&r, "[::1]:8080", Some("http://[::1]:8080")));
        assert!(served(&r, "100.64.0.2:8080", Some("http://100.64.0.2:8080")));
        // Programs (glass mcp, curl) send no Origin.
        assert!(served(&r, "box.tail1.ts.net", None));
    }

    #[test]
    fn oauth_and_mcp_keep_their_own_cross_site_rules() {
        let r = glass();
        let host = Some("box.tail1.ts.net:8443");
        // Metadata, registration and tokens are open to any site (no cookies there).
        for path in ["/.well-known/oauth-authorization-server", "/oauth/register", "/oauth/token", "/oauth/revoke"] {
            assert!(r.refuse(path, host, Some("https://agent.example")).is_none(), "{path}");
        }
        // Approving isn't.
        assert!(r.refuse("/oauth/authorize/decide", host, Some("https://agent.example")).is_some());
        assert!(r.refuse("/oauth/authorize/decide", host, Some("https://box.tail1.ts.net:8443")).is_none());
        // /mcp: pages on this device's loopback (MCP Inspector) may call it, others not.
        assert!(r.refuse("/mcp", host, Some("http://localhost:6274")).is_none());
        assert!(r.refuse("/mcp", host, Some("http://127.0.0.1:6274")).is_none());
        assert!(r.refuse("/mcp", host, Some("https://agent.example")).is_some());
        assert!(r.refuse("/agent/v1/act", host, Some("http://localhost:6274")).is_some());
        // Still only under this computer's names.
        assert!(r.refuse("/oauth/token", Some("attacker.example"), Some("https://agent.example")).is_some());
    }

    #[test]
    fn allowed_hosts_and_origins_are_served() {
        // Behind the owner's own proxy, under their domain.
        let r = Rules::new(&[], &[".example.com".into(), "pc.corp:8080".into()], &["https://app.example.org/".into()]);
        assert!(served(&r, "glass.example.com", Some("https://glass.example.com")));
        assert!(served(&r, "example.com", None));
        assert!(served(&r, "pc.corp:8080", Some("http://pc.corp:8080")));
        assert!(!served(&r, "notexample.com", None));
        assert!(served(&r, "127.0.0.1:8080", Some("https://app.example.org")));
        assert!(served(&r, "127.0.0.1:8080", Some("https://app.example.org:443")));
        assert!(!served(&r, "127.0.0.1:8080", Some("http://app.example.org")));
        // The proxied page's origin has to match the host it asked for.
        assert!(!served(&r, "a.example.com", Some("https://b.example.com")));
        let any = Rules::new(&[], &["*".into()], &["*".into()]);
        assert!(served(&any, "attacker.example", Some("https://attacker.example")));
    }

    #[test]
    fn webtransport_sessions_come_from_glass_pages() {
        let r = glass();
        let ok = |authority: &str, origin: Option<&str>| r.refuse_session(authority, origin).is_none();
        // This host's own page, on its web port.
        assert!(ok("box.tail1.ts.net:4433", Some("https://box.tail1.ts.net")));
        assert!(ok("localhost:4433", Some("http://localhost:8080")));
        // The viewer on another of the owner's machines, or on localhost, switching here.
        assert!(ok("box.tail1.ts.net:4433", Some("https://studio.tail1.ts.net")));
        assert!(ok("127.0.0.1:4433", Some("http://localhost:8080")));
        assert!(ok("100.64.0.2:4433", Some("https://100.64.0.9:8080")));
        // The remote-access bridge and other programs send no Origin.
        assert!(ok("127.0.0.1:4433", None));
        // Any other site's page: refused, even with this computer's certificate hash.
        assert!(!ok("box.tail1.ts.net:4433", Some("https://attacker.example")));
        assert!(!ok("100.64.0.2:4433", Some("https://1.2.3.4")));
        assert!(!ok("box.tail1.ts.net:4433", Some("https://studio.tail2.ts.net")));
        assert!(!ok("box.tail1.ts.net:4433", Some("null")));
        // A rebound name, with a page on that same name.
        assert!(!ok("attacker.example:4433", Some("https://attacker.example")));
    }

    #[test]
    fn json_bodies_must_say_so() {
        let with = |ct: Option<&str>| {
            let mut h = HeaderMap::new();
            if let Some(ct) = ct {
                h.insert(header::CONTENT_TYPE, ct.parse().unwrap());
            }
            require_json(&h).is_none()
        };
        assert!(with(Some("application/json")));
        assert!(with(Some("Application/JSON; charset=utf-8")));
        assert!(!with(Some("text/plain")));
        assert!(!with(Some("application/x-www-form-urlencoded")));
        assert!(!with(None));
    }

    #[test]
    fn names_come_from_urls() {
        assert_eq!(url_host("https://Box.tail1.ts.net:8443/").as_deref(), Some("box.tail1.ts.net"));
        assert_eq!(url_host("http://localhost:8080/").as_deref(), Some("localhost"));
        assert_eq!(url_host("http://[::1]:8080/").as_deref(), Some("::1"));
        assert_eq!(Rules::new(&["box.tail1.ts.net".into(), "glass.example.com".into()], &[], &[]).tailnets, ["tail1.ts.net"]);
    }
}
