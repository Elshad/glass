package main

import (
	"errors"
	"fmt"
	"net"
	"net/netip"
	"strconv"
	"strings"
)

// An allowlist is the fixed set of destinations this node may open
// connections to. It is checked for every dial, so it also covers redirects
// and anything else that makes the HTTP client connect somewhere new.
//
// It is a second fence, not the first: the tailnet's ACLs decide what the node
// can reach. The allowlist keeps a bug or an attacker-supplied URL in the page
// from reaching everything those ACLs happen to permit.
type allowlist []allowRule

type allowRule struct {
	// Exactly one of name, suffix or prefix is set.
	name    string       // exact host name, lower case, no trailing dot
	suffix  string       // ".example.ts.net": any host name ending in it
	prefix  netip.Prefix // an address or a CIDR range
	anyPort bool
	port    uint16
}

var errNotAllowed = errors.New("destination not allowed")

// parseAllowlist parses entries of the form host:port, where host is an IP
// address, a CIDR range, a host name or "*.suffix", and port is a number or
// "*". IPv6 addresses and ranges go in brackets: "[fd7a:115c:a1e0::1]:443".
func parseAllowlist(entries []string) (allowlist, error) {
	if len(entries) == 0 {
		return nil, errors.New("allow list is empty: nothing would be reachable")
	}
	list := make(allowlist, 0, len(entries))
	for _, e := range entries {
		r, err := parseAllowRule(e)
		if err != nil {
			return nil, fmt.Errorf("allow entry %q: %w", e, err)
		}
		list = append(list, r)
	}
	return list, nil
}

func parseAllowRule(entry string) (allowRule, error) {
	var r allowRule
	entry = strings.TrimSpace(entry)
	i := strings.LastIndexByte(entry, ':')
	if i <= 0 {
		return r, errors.New(`needs a port: use "host:port" or "host:*"`)
	}
	host, port := entry[:i], entry[i+1:]

	if port == "*" {
		r.anyPort = true
	} else {
		p, err := strconv.ParseUint(port, 10, 16)
		if err != nil || p == 0 {
			return r, fmt.Errorf("bad port %q", port)
		}
		r.port = uint16(p)
	}

	bracketed := strings.HasPrefix(host, "[") && strings.HasSuffix(host, "]")
	if bracketed {
		host = host[1 : len(host)-1]
	} else if strings.Contains(host, ":") {
		return r, errors.New("put IPv6 addresses in brackets")
	}

	if strings.Contains(host, "/") {
		p, err := netip.ParsePrefix(host)
		if err != nil {
			return r, fmt.Errorf("bad range: %w", err)
		}
		r.prefix = netip.PrefixFrom(p.Addr().Unmap(), p.Bits()).Masked()
		return r, nil
	}
	if a, err := netip.ParseAddr(host); err == nil {
		a = a.Unmap().WithZone("")
		r.prefix = netip.PrefixFrom(a, a.BitLen())
		return r, nil
	}
	if bracketed {
		return r, errors.New("brackets are for IPv6 addresses")
	}

	host = normalizeName(host)
	if rest, ok := strings.CutPrefix(host, "*."); ok {
		if !validName(rest) {
			return r, errors.New("bad host name")
		}
		r.suffix = "." + rest
		return r, nil
	}
	if !validName(host) {
		return r, errors.New("bad host name")
	}
	r.name = host
	return r, nil
}

// check reports whether a connection to hostport ("host:port", as net/http
// passes it to a dialer) is allowed.
func (l allowlist) check(hostport string) error {
	host, portStr, err := net.SplitHostPort(hostport)
	if err != nil {
		return fmt.Errorf("%w: %q", errNotAllowed, hostport)
	}
	p, err := strconv.ParseUint(portStr, 10, 16)
	if err != nil || p == 0 {
		return fmt.Errorf("%w: %s", errNotAllowed, hostport)
	}
	port := uint16(p)

	addr, addrErr := netip.ParseAddr(host)
	isAddr := addrErr == nil
	if isAddr {
		addr = addr.Unmap().WithZone("")
	} else {
		host = normalizeName(host)
	}

	for _, r := range l {
		if !r.anyPort && r.port != port {
			continue
		}
		switch {
		case isAddr:
			if r.prefix.IsValid() && r.prefix.Contains(addr) {
				return nil
			}
		case r.name != "":
			if r.name == host {
				return nil
			}
		case r.suffix != "":
			if strings.HasSuffix(host, r.suffix) && len(host) > len(r.suffix) {
				return nil
			}
		}
	}
	return fmt.Errorf("%w: %s", errNotAllowed, hostport)
}

func normalizeName(h string) string {
	return strings.TrimSuffix(strings.ToLower(h), ".")
}

func validName(h string) bool {
	if h == "" || len(h) > 253 {
		return false
	}
	for _, label := range strings.Split(h, ".") {
		if label == "" || len(label) > 63 {
			return false
		}
		for _, c := range label {
			ok := c >= 'a' && c <= 'z' || c >= '0' && c <= '9' || c == '-' || c == '_'
			if !ok {
				return false
			}
		}
	}
	return true
}
