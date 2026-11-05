//! Addresses the HTTP, HTTPS and WebTransport listeners bind.
//!
//! With no `--bind` and no `glass config bind`, TCP stays on `0.0.0.0` and WebTransport
//! keeps the dual-stack socket it has always used. An explicit list is used for every
//! listener, including a literal `0.0.0.0` (IPv4 only, on all three).
//!
//! The agent API (`127.0.0.1`) and forwarded-port connections to local servers are
//! separate sockets and do not follow this list.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener};

use anyhow::Result;

/// Where the public listeners bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// TCP on `0.0.0.0`. WebTransport uses its dual-stack default (IPv4 and IPv6).
    Default,
    /// These addresses, on every listener. IPv6 sockets are IPv6-only, so `::` does not
    /// cover IPv4 and can be combined with `0.0.0.0`.
    Explicit(Vec<IpAddr>),
}

impl Listen {
    /// CLI `--bind` wins when it is present; otherwise the saved `glass config bind` list.
    /// An empty list is [`Listen::Default`].
    pub async fn resolve(cli: &[String], saved: &[String]) -> Result<Self> {
        let tokens = parse_bind(if cli.is_empty() { saved } else { cli })?;
        if tokens.is_empty() {
            return Ok(Self::Default);
        }
        let fetched;
        let tailscale_ips: &[IpAddr] = if tokens.iter().any(|t| t == "tailscale") {
            fetched = crate::tailscale::self_ips().await?;
            &fetched
        } else {
            &[]
        };
        Ok(Self::Explicit(resolve_ips(&tokens, tailscale_ips)?))
    }

    pub fn tcp_ips(&self) -> Vec<IpAddr> {
        match self {
            Self::Default => vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)],
            Self::Explicit(ips) => ips.clone(),
        }
    }

    /// `tailscale serve` forwards to `localhost`, which is IPv4 loopback here.
    pub fn covers_ipv4_loopback(&self) -> bool {
        match self {
            Self::Default => true,
            Self::Explicit(ips) => ips.iter().any(|ip| {
                *ip == IpAddr::V4(Ipv4Addr::UNSPECIFIED) || *ip == IpAddr::V4(Ipv4Addr::LOCALHOST)
            }),
        }
    }

    /// Address the in-process WebRTC bridge dials. Loopback when this process can reach
    /// it, otherwise the first bound address (a Tailscale address on this machine).
    pub fn wt_dial(&self) -> IpAddr {
        let Self::Explicit(ips) = self else {
            return IpAddr::V4(Ipv4Addr::LOCALHOST);
        };
        if ips.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST))
            || ips.contains(&IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        {
            return IpAddr::V4(Ipv4Addr::LOCALHOST);
        }
        if ips.contains(&IpAddr::V6(Ipv6Addr::LOCALHOST))
            || ips.contains(&IpAddr::V6(Ipv6Addr::UNSPECIFIED))
        {
            return IpAddr::V6(Ipv6Addr::LOCALHOST);
        }
        ips[0]
    }
}

/// `127.0.0.1` or `[::1]`, for URLs.
pub fn host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    }
}

pub fn describe(listen: &Listen) -> String {
    match listen {
        Listen::Default => "0.0.0.0".into(),
        Listen::Explicit(ips) => ips.iter().copied().map(host).collect::<Vec<_>>().join(", "),
    }
}

/// Repeatable CLI values, or one config string. Commas and spaces separate addresses.
/// `tailscale` is the only non-IP token. An empty input is the default (no tokens).
pub fn parse_bind<I, S>(values: I) -> Result<Vec<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for value in values {
        for part in value.as_ref().split([',', ' ', '\t']) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let token = canonical_token(part)?;
            if !out.iter().any(|t| t == &token) {
                out.push(token);
            }
        }
    }
    Ok(out)
}

fn canonical_token(raw: &str) -> Result<String> {
    if raw.eq_ignore_ascii_case("tailscale") {
        return Ok("tailscale".into());
    }
    if raw.parse::<std::net::SocketAddr>().is_ok() {
        anyhow::bail!(
            "{raw} includes a port; choose it with --http-port, --https-port or --wt-port"
        );
    }
    let bare = raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw);
    match bare.parse::<IpAddr>() {
        Ok(ip) => Ok(ip.to_string()),
        Err(_) => anyhow::bail!("{raw} isn't an IP address or tailscale"),
    }
}

/// Replaces `tailscale` with `tailscale_ips` and rejects a wildcard mixed with a
/// specific address of the same family.
pub fn resolve_ips(tokens: &[String], tailscale_ips: &[IpAddr]) -> Result<Vec<IpAddr>> {
    let mut ips = Vec::new();
    for token in tokens {
        if token == "tailscale" {
            anyhow::ensure!(
                !tailscale_ips.is_empty(),
                "Tailscale isn't up, so there's no Tailscale address to listen on. Run `tailscale up`, or pass an IP address."
            );
            for ip in tailscale_ips {
                push_unique(&mut ips, *ip);
            }
        } else {
            let ip = token
                .parse::<IpAddr>()
                .map_err(|_| anyhow::anyhow!("{token} isn't an IP address or tailscale"))?;
            push_unique(&mut ips, ip);
        }
    }
    anyhow::ensure!(!ips.is_empty(), "expected an IP address or tailscale");
    let v4_any = ips.contains(&IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let v6_any = ips.contains(&IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    if v4_any && ips.iter().any(|ip| ip.is_ipv4() && !ip.is_unspecified()) {
        anyhow::bail!(
            "0.0.0.0 already covers every IPv4 address; drop it or the other IPv4 addresses"
        );
    }
    if v6_any && ips.iter().any(|ip| ip.is_ipv6() && !ip.is_unspecified()) {
        anyhow::bail!(":: already covers every IPv6 address; drop it or the other IPv6 addresses");
    }
    Ok(ips)
}

fn push_unique(ips: &mut Vec<IpAddr>, ip: IpAddr) {
    if !ips.contains(&ip) {
        ips.push(ip);
    }
}

/// Binds every address on one port. A failure drops any sockets already opened.
pub fn bind_port(ips: &[IpAddr], port: u16) -> std::io::Result<Vec<TcpListener>> {
    let mut listeners = Vec::with_capacity(ips.len());
    for ip in ips {
        match TcpListener::bind((*ip, port)) {
            Ok(listener) => listeners.push(listener),
            Err(e) => {
                return Err(std::io::Error::new(e.kind(), format!("{ip}:{port}: {e}")));
            }
        }
    }
    Ok(listeners)
}

/// `port`, or the next free one within [`crate::PORT_TRIES`], on every listen address.
/// The same port is used for each address so clients have one number to try.
pub fn bind_tcp(listen: &Listen, first_port: u16) -> Result<Vec<TcpListener>> {
    let ips = listen.tcp_ips();
    let last = first_port.saturating_add(crate::PORT_TRIES - 1);
    let mut last_err = None;
    for port in first_port..=last {
        match bind_port(&ips, port) {
            Ok(listeners) => return Ok(listeners),
            Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
                anyhow::bail!("can't listen on {e}. Check --bind or `glass config bind`.")
            }
            Err(e) => last_err = Some(e),
        }
    }
    let why = last_err.map(|e| format!(" ({e})")).unwrap_or_default();
    anyhow::bail!("TCP ports {first_port}–{last} are all in use{why}; free one or pass --http-port")
}

/// The standard HTTPS port, on every listen address. An empty list means "not this
/// time" (port 0, or the port is taken): links then include the web port. An address
/// that isn't on this machine is reported instead of skipped.
pub fn bind_https(listen: &Listen, port: u16) -> Result<Vec<TcpListener>> {
    if port == 0 {
        return Ok(Vec::new());
    }
    match bind_port(&listen.tcp_ips(), port) {
        Ok(listeners) => Ok(listeners),
        Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {
            anyhow::bail!("can't listen on {e}. Check --bind or `glass config bind`.")
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                "port {port} refused ({e}). On macOS, port 443 needs the default bind (or 0.0.0.0); links include the web port."
            );
            Ok(Vec::new())
        }
        Err(e) => {
            tracing::info!("port {port} unavailable ({e}); links include the web port");
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_repeated_values_commas_and_brackets() {
        let tokens = parse_bind(["127.0.0.1, ::1", "tailscale", "127.0.0.1"]).unwrap();
        assert_eq!(tokens, vec!["127.0.0.1", "::1", "tailscale"]);
    }

    #[test]
    fn tailscale_keyword_is_case_insensitive() {
        assert_eq!(parse_bind(["Tailscale"]).unwrap(), vec!["tailscale"]);
    }

    #[test]
    fn rejects_ports_names_and_blank_lists() {
        assert!(parse_bind(["127.0.0.1:8080"]).is_err());
        assert!(parse_bind(["[::1]:443"]).is_err());
        assert!(parse_bind(["localhost"]).is_err());
        assert!(parse_bind(["not-an-ip"]).is_err());
        assert!(parse_bind([""]).unwrap().is_empty());
    }

    #[test]
    fn tailscale_expands_and_dedups() {
        let tokens = parse_bind(["tailscale, 127.0.0.1"]).unwrap();
        let ips = resolve_ips(
            &tokens,
            &[v4("100.64.0.2"), "fd7a:115c:a1e0::1".parse().unwrap()],
        )
        .unwrap();
        assert_eq!(
            ips,
            vec![
                v4("100.64.0.2"),
                "fd7a:115c:a1e0::1".parse().unwrap(),
                v4("127.0.0.1"),
            ]
        );
        let again = resolve_ips(&tokens, &[v4("100.64.0.2"), v4("100.64.0.2")]).unwrap();
        assert_eq!(again, vec![v4("100.64.0.2"), v4("127.0.0.1")]);
    }

    #[test]
    fn tailscale_without_addresses_fails() {
        let tokens = parse_bind(["tailscale"]).unwrap();
        let err = resolve_ips(&tokens, &[]).unwrap_err().to_string();
        assert!(err.contains("Tailscale isn't up"), "{err}");
    }

    #[test]
    fn wildcard_cannot_share_a_family_with_a_specific_address() {
        let both = parse_bind(["0.0.0.0, 127.0.0.1"]).unwrap();
        assert!(resolve_ips(&both, &[]).is_err());
        let v6 = parse_bind(["::, ::1"]).unwrap();
        assert!(resolve_ips(&v6, &[]).is_err());
        let split = parse_bind(["0.0.0.0, ::1"]).unwrap();
        assert!(resolve_ips(&split, &[]).is_ok());
    }

    #[test]
    fn dial_and_loopback_follow_the_list() {
        assert_eq!(Listen::Default.wt_dial(), v4("127.0.0.1"));
        assert!(Listen::Default.covers_ipv4_loopback());
        let ts = Listen::Explicit(vec![v4("100.64.0.2")]);
        assert_eq!(ts.wt_dial(), v4("100.64.0.2"));
        assert!(!ts.covers_ipv4_loopback());
        let both = Listen::Explicit(vec![v4("100.64.0.2"), v4("127.0.0.1")]);
        assert_eq!(both.wt_dial(), v4("127.0.0.1"));
        assert!(both.covers_ipv4_loopback());
        let v6 = Listen::Explicit(vec![IpAddr::V6(Ipv6Addr::UNSPECIFIED)]);
        assert_eq!(v6.wt_dial(), IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(host(v6.wt_dial()), "[::1]");
    }

    #[test]
    fn binds_each_address_and_moves_to_the_next_free_port() {
        let listen = Listen::Explicit(vec![v4("127.0.0.1")]);
        let hold = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = hold.local_addr().unwrap().port();
        let bound = bind_tcp(&listen, taken).unwrap();
        let got = bound[0].local_addr().unwrap().port();
        assert!(
            got > taken && got < taken.saturating_add(crate::PORT_TRIES),
            "{got}"
        );
        assert_eq!(bound[0].local_addr().unwrap().ip(), v4("127.0.0.1"));
    }

    #[test]
    fn binds_v4_and_v6_loopback_on_the_same_port() {
        if TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_err() {
            return;
        }
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let ips = [v4("127.0.0.1"), IpAddr::V6(Ipv6Addr::LOCALHOST)];
        let listeners = bind_port(&ips, port).unwrap();
        let got: Vec<_> = listeners
            .iter()
            .map(|l| l.local_addr().unwrap().ip())
            .collect();
        assert_eq!(got, ips);
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().port() == port)
        );
    }
}
