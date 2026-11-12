package main

import "testing"

func TestAllowlist(t *testing.T) {
	list, err := parseAllowlist([]string{
		"100.80.10.20:3000",
		"100.64.0.0/10:8080",
		"api.tail1234.ts.net:443",
		"*.svc.tail1234.ts.net:*",
		"[fd7a:115c:a1e0::1]:443",
		"[fd7a:115c:a1e0:ab12::/64]:22",
	})
	if err != nil {
		t.Fatal(err)
	}
	for hostport, want := range map[string]bool{
		"100.80.10.20:3000":              true,
		"100.80.10.20:3001":              false,
		"100.80.10.21:3000":              false,
		"100.99.1.2:8080":                true,
		"101.0.0.1:8080":                 false,
		"[::ffff:100.80.10.20]:3000":     true,
		"api.tail1234.ts.net:443":        true,
		"API.Tail1234.ts.net.:443":       true,
		"api.tail1234.ts.net:80":         false,
		"evil-api.tail1234.ts.net:443":   false,
		"a.svc.tail1234.ts.net:9":        true,
		"svc.tail1234.ts.net:9":          false,
		"xsvc.tail1234.ts.net:9":         false,
		"a.svc.tail1234.ts.net.evil:9":   false,
		"[fd7a:115c:a1e0::1]:443":        true,
		"[fd7a:115c:a1e0::2]:443":        false,
		"[fd7a:115c:a1e0:ab12::9]:22":    true,
		"127.0.0.1:3000":                 false,
		"100.80.10.20":                   false,
		"100.80.10.20:0":                 false,
		"100.80.10.20:99999":             false,
		"":                               false,
		"1684540948:3000":                false, // 100.80.10.20 as one number
		"100.80.10.20.evil.example:3000": false,
	} {
		if got := list.check(hostport) == nil; got != want {
			t.Errorf("check(%q) = %v, want %v", hostport, got, want)
		}
	}
}

func TestAllowlistRejects(t *testing.T) {
	for _, e := range []string{
		"", "100.80.10.20", "host", "*:80", "*:*", "*.:80", "host:0", "host:http",
		"fd7a::1:443", "[host]:80", "[fd7a::1]", "100.64.0.0/40:80", "ho st:80",
		"http://host:80", "host:80/path",
	} {
		if _, err := parseAllowlist([]string{e}); err == nil {
			t.Errorf("parseAllowlist(%q) succeeded, want an error", e)
		}
	}
	if _, err := parseAllowlist(nil); err == nil {
		t.Error("an empty list succeeded, want an error")
	}
}
