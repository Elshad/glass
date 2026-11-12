//go:build js && wasm

// The tsfetch WebAssembly module: an ephemeral Tailscale node that lives in a
// browser tab and makes HTTP requests through its own userspace network stack.
//
// It is started by tsfetch.js, which passes the name of a global callback as
// the first argument. The module calls it once with the bridge object below
// and then stays alive until stop() is called.
//
// Derived from tailscale.com/cmd/tsconnect/wasm (BSD-3-Clause), with SSH, log
// upload and persistent state removed, and fetch extended to full requests
// behind a destination allowlist.
package main

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"os"
	"strings"
	"sync"
	"syscall/js"
	"time"

	"tailscale.com/control/controlclient"
	"tailscale.com/ipn"
	"tailscale.com/ipn/ipnauth"
	"tailscale.com/ipn/ipnlocal"
	"tailscale.com/ipn/ipnserver"
	"tailscale.com/ipn/store/mem"
	"tailscale.com/net/bakedroots"
	"tailscale.com/net/netns"
	"tailscale.com/net/tsdial"
	"tailscale.com/safesocket"
	"tailscale.com/tsd"
	"tailscale.com/types/logger"
	"tailscale.com/types/logid"
	"tailscale.com/wgengine"
	"tailscale.com/wgengine/netstack"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Println("tsfetch: started without a handshake callback")
		return
	}
	b := &bridge{done: make(chan struct{})}
	js.Global().Call(os.Args[1], b.jsAPI())
	<-b.done
}

// bridge is what JavaScript holds. One bridge runs at most one node.
type bridge struct {
	done chan struct{}

	mu      sync.Mutex
	node    *node
	stopped bool
}

func (b *bridge) jsAPI() map[string]any {
	return map[string]any{
		// start(config): Promise<void>. Resolves once the node is starting;
		// progress arrives through config.onState and friends.
		"start": js.FuncOf(func(this js.Value, args []js.Value) any {
			if len(args) != 1 {
				return rejected("start(config) takes one argument")
			}
			cfg, err := readConfig(args[0])
			return makePromise(func() (any, error) {
				if err != nil {
					return nil, err
				}
				b.mu.Lock()
				defer b.mu.Unlock()
				if b.node != nil || b.stopped {
					return nil, errors.New("already started")
				}
				n, err := startNode(cfg)
				if err != nil {
					return nil, err
				}
				b.node = n
				return nil, nil
			})
		}),
		// fetch(request): {promise, abort}
		"fetch": js.FuncOf(func(this js.Value, args []js.Value) any {
			b.mu.Lock()
			n := b.node
			b.mu.Unlock()
			if n == nil || len(args) != 1 {
				return map[string]any{
					"promise": rejected("not connected"),
					"abort":   js.FuncOf(func(js.Value, []js.Value) any { return nil }),
				}
			}
			return n.fetch(args[0])
		}),
		// self(): JSON string {name, addresses}
		"self": js.FuncOf(func(this js.Value, args []js.Value) any {
			b.mu.Lock()
			n := b.node
			b.mu.Unlock()
			if n == nil {
				return "{}"
			}
			return n.self()
		}),
		// stop(): Promise<void>. Logs the node out (which deletes an
		// ephemeral node from the tailnet at once) and ends the program.
		"stop": js.FuncOf(func(this js.Value, args []js.Value) any {
			return makePromise(func() (any, error) {
				b.mu.Lock()
				n := b.node
				already := b.stopped
				b.node, b.stopped = nil, true
				b.mu.Unlock()
				if already {
					return nil, nil
				}
				if n != nil {
					n.stop()
				}
				// Let the promise settle before the program exits and every
				// exported function stops working.
				time.AfterFunc(50*time.Millisecond, func() { close(b.done) })
				return nil, nil
			})
		}),
	}
}

type config struct {
	authKey     string
	controlURL  string
	hostname    string
	allow       allowlist
	interactive bool
	logf        logger.Logf

	onState    js.Value
	onLoginURL js.Value
	onError    js.Value
}

func readConfig(v js.Value) (*config, error) {
	str := func(name string) string {
		if f := v.Get(name); f.Type() == js.TypeString {
			return f.String()
		}
		return ""
	}
	c := &config{
		authKey:     str("authKey"),
		controlURL:  str("controlURL"),
		hostname:    str("hostname"),
		interactive: v.Get("interactive").Truthy(),
		onState:     v.Get("onState"),
		onLoginURL:  v.Get("onLoginURL"),
		onError:     v.Get("onError"),
		logf:        logger.Discard,
	}
	if c.controlURL == "" {
		c.controlURL = ipn.DefaultControlURL
	}
	if c.authKey == "" && !c.interactive {
		return nil, errors.New("no auth key")
	}
	if v.Get("debug").Truthy() {
		c.logf = log.Printf
	}
	jsAllow := v.Get("allow")
	if jsAllow.Type() != js.TypeObject {
		return nil, errors.New("allow must be a list of host:port entries")
	}
	entries := make([]string, jsAllow.Length())
	for i := range entries {
		e := jsAllow.Index(i)
		if e.Type() != js.TypeString {
			return nil, errors.New("allow must be a list of host:port entries")
		}
		entries[i] = e.String()
	}
	var err error
	c.allow, err = parseAllowlist(entries)
	if err != nil {
		return nil, err
	}
	return c, nil
}

type node struct {
	cfg       *config
	dialer    *tsdial.Dialer
	lb        *ipnlocal.LocalBackend
	transport *http.Transport

	ctx    context.Context // done when the node stops
	cancel context.CancelFunc
}

var jsState = map[ipn.State]string{
	ipn.NoState:          "NoState",
	ipn.InUseOtherUser:   "InUseOtherUser",
	ipn.NeedsLogin:       "NeedsLogin",
	ipn.NeedsMachineAuth: "NeedsMachineAuth",
	ipn.Stopped:          "Stopped",
	ipn.Starting:         "Starting",
	ipn.Running:          "Running",
}

func startNode(cfg *config) (*node, error) {
	logf := cfg.logf
	netns.SetEnabled(false)

	// State lives in memory only: closing the tab throws the node key away,
	// and the next connect is a new node.
	store := new(mem.Store)

	sys := tsd.NewSystem()
	sys.Set(store)
	dialer := &tsdial.Dialer{Logf: logf}
	dialer.SetBus(sys.Bus.Get())
	eng, err := wgengine.NewUserspaceEngine(logf, wgengine.Config{
		Dialer:        dialer,
		SetSubsystem:  sys.Set,
		ControlKnobs:  sys.ControlKnobs(),
		HealthTracker: sys.HealthTracker.Get(),
		ExtraRootCAs:  sys.ExtraRootCAs,
		Metrics:       sys.UserMetricsRegistry(),
		EventBus:      sys.Bus.Get(),
	})
	if err != nil {
		return nil, fmt.Errorf("starting the WireGuard engine: %w", err)
	}
	sys.Set(eng)

	ns, err := netstack.Create(logf, sys.Tun.Get(), eng, sys.MagicSock.Get(), dialer, sys.DNSManager.Get(), sys.ProxyMapper())
	if err != nil {
		return nil, fmt.Errorf("creating the network stack: %w", err)
	}
	sys.Set(ns)
	ns.ProcessLocalIPs = true
	ns.ProcessSubnets = true

	// Everything this node dials goes through the userspace stack. There is
	// no other network to fall back to in a browser.
	dialer.UseNetstackForIP = func(netip.Addr) bool { return true }
	dialer.NetstackDialTCP = func(ctx context.Context, dst netip.AddrPort) (net.Conn, error) {
		c, err := ns.DialContextTCP(ctx, dst)
		if err != nil {
			return nil, err
		}
		return c, nil
	}
	dialer.NetstackDialUDP = func(ctx context.Context, dst netip.AddrPort) (net.Conn, error) {
		c, err := ns.DialContextUDP(ctx, dst)
		if err != nil {
			return nil, err
		}
		return c, nil
	}
	sys.NetstackRouter.Set(true)
	sys.Tun.Get().Start()

	var logID logid.PublicID
	srv := ipnserver.New(logf, logID, sys.Bus.Get(), sys.NetMon.Get())
	lb, err := ipnlocal.NewLocalBackend(logf, logID, sys, controlclient.LoginEphemeral)
	if err != nil {
		return nil, fmt.Errorf("creating the backend: %w", err)
	}
	if err := ns.Start(lb); err != nil {
		return nil, fmt.Errorf("starting the network stack: %w", err)
	}
	srv.SetLocalBackend(lb)

	ctx, cancel := context.WithCancel(context.Background())
	n := &node{cfg: cfg, dialer: dialer, lb: lb, ctx: ctx, cancel: cancel}
	n.transport = &http.Transport{
		// Setting DialContext is also what makes net/http use a real HTTP
		// client here instead of handing the request to the browser's fetch.
		DialContext: n.dial,
		// A browser has no system root store to read. These are the roots
		// that sign *.ts.net certificates (Let's Encrypt).
		TLSClientConfig:       &tls.Config{RootCAs: bakedroots.Get()},
		MaxIdleConnsPerHost:   4,
		IdleConnTimeout:       60 * time.Second,
		TLSHandshakeTimeout:   15 * time.Second,
		ResponseHeaderTimeout: 0,
	}

	var loginOnce sync.Once
	lb.SetNotifyCallback(func(nt ipn.Notify) {
		defer func() {
			if r := recover(); r != nil {
				fmt.Println("tsfetch: recovered in notify:", r)
			}
		}()
		if nt.ErrMessage != nil {
			call(cfg.onError, *nt.ErrMessage)
		}
		if nt.BrowseToURL != nil {
			call(cfg.onLoginURL, *nt.BrowseToURL)
		}
		if nt.State != nil {
			call(cfg.onState, jsState[*nt.State])
			if *nt.State == ipn.NeedsLogin {
				// Registers with the auth key when there is one; otherwise
				// asks control for a sign-in URL (onLoginURL).
				loginOnce.Do(func() { go lb.StartLoginInteractive(ctx) })
			}
		}
	})

	go func() {
		err := lb.Start(ipn.Options{
			UpdatePrefs: &ipn.Prefs{
				ControlURL:  cfg.controlURL,
				RouteAll:    false,
				WantRunning: true,
				Hostname:    cfg.hostname,
			},
			AuthKey: cfg.authKey,
		})
		if err != nil {
			call(cfg.onError, "start: "+err.Error())
		}
	}()

	go func() {
		ln, err := safesocket.Listen("")
		if err != nil {
			logf("tsfetch: safesocket.Listen: %v", err)
			return
		}
		err = srv.Run(ctx, ln)
		logf("tsfetch: ipnserver stopped: %v", err)
	}()

	return n, nil
}

// dial is the only way this node opens a connection for a request.
func (n *node) dial(ctx context.Context, network, addr string) (net.Conn, error) {
	if err := n.cfg.allow.check(addr); err != nil {
		return nil, err
	}
	return n.dialer.UserDial(ctx, network, addr)
}

func (n *node) self() string {
	out := struct {
		Name      string   `json:"name"`
		Addresses []string `json:"addresses"`
	}{Addresses: []string{}}
	if nm := n.lb.NetMapWithPeers(); nm != nil {
		out.Name = strings.TrimSuffix(nm.SelfName(), ".")
		for _, p := range nm.GetAddresses().All() {
			out.Addresses = append(out.Addresses, p.Addr().String())
		}
	}
	j, _ := json.Marshal(out)
	return string(j)
}

func (n *node) stop() {
	n.cancel()
	n.transport.CloseIdleConnections()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := n.lb.Logout(ctx, ipnauth.Self); err != nil {
		n.cfg.logf("tsfetch: logout: %v", err)
	}
	n.lb.Shutdown()
}

// fetch runs one request. jsReq is
//
//	{url, method, headers: [[name, value], ...], body?: Uint8Array, redirect}
//
// and the promise resolves to
//
//	{status, statusText, url, redirected, headers: [[name, value], ...],
//	 read(): Promise<Uint8Array|null>, cancel()}
func (n *node) fetch(jsReq js.Value) map[string]any {
	ctx, cancel := context.WithCancel(n.ctx)

	rawURL := jsReq.Get("url").String()
	method := jsReq.Get("method").String()
	redirect := jsReq.Get("redirect").String()

	var headers [][2]string
	if h := jsReq.Get("headers"); h.Type() == js.TypeObject {
		for i := range h.Length() {
			pair := h.Index(i)
			headers = append(headers, [2]string{pair.Index(0).String(), pair.Index(1).String()})
		}
	}
	var body []byte
	if b := jsReq.Get("body"); b.Type() == js.TypeObject {
		body = make([]byte, b.Get("byteLength").Int())
		js.CopyBytesToGo(body, b)
	}

	var abort js.Func
	abort = js.FuncOf(func(js.Value, []js.Value) any {
		cancel()
		return nil
	})

	promise := makePromise(func() (any, error) {
		res, err := n.roundTrip(ctx, rawURL, method, redirect, headers, body)
		if err != nil {
			cancel()
			abort.Release()
			if errors.Is(err, context.Canceled) {
				return nil, errors.New("aborted")
			}
			return nil, err
		}
		return responseObject(res, func() {
			cancel()
			abort.Release()
		}), nil
	})
	return map[string]any{"promise": promise, "abort": abort}
}

func (n *node) roundTrip(ctx context.Context, rawURL, method, redirect string, headers [][2]string, body []byte) (*http.Response, error) {
	u, err := url.Parse(rawURL)
	if err != nil {
		return nil, fmt.Errorf("bad URL: %w", err)
	}
	if err := n.checkURL(u); err != nil {
		return nil, err
	}

	var rd io.Reader
	if len(body) > 0 {
		rd = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, u.String(), rd)
	if err != nil {
		return nil, err
	}
	for _, h := range headers {
		if strings.EqualFold(h[0], "host") {
			// The Host header always follows the URL, so an allowed address
			// can't be used to reach a different virtual host by accident.
			continue
		}
		req.Header.Add(h[0], h[1])
	}

	client := &http.Client{
		Transport: n.transport,
		CheckRedirect: func(next *http.Request, via []*http.Request) error {
			switch redirect {
			case "manual":
				return http.ErrUseLastResponse
			case "error":
				return errors.New("redirected, and redirect is set to \"error\"")
			}
			if len(via) >= 20 {
				return errors.New("too many redirects")
			}
			return n.checkURL(next.URL)
		},
	}
	return client.Do(req)
}

func (n *node) checkURL(u *url.URL) error {
	port := u.Port()
	switch u.Scheme {
	case "http":
		if port == "" {
			port = "80"
		}
	case "https":
		if port == "" {
			port = "443"
		}
	default:
		return fmt.Errorf("unsupported URL scheme %q", u.Scheme)
	}
	if u.User != nil {
		return errors.New("URLs with a user name or password are not supported")
	}
	return n.cfg.allow.check(net.JoinHostPort(u.Hostname(), port))
}

func responseObject(res *http.Response, done func()) map[string]any {
	var headers []any
	for name, values := range res.Header {
		for _, v := range values {
			headers = append(headers, []any{name, v})
		}
	}
	if headers == nil {
		headers = []any{}
	}

	var read, cancelBody js.Func
	var once sync.Once
	finish := func() {
		once.Do(func() {
			res.Body.Close()
			done()
			// Released a moment later: this may be running inside one of them.
			time.AfterFunc(time.Second, func() {
				read.Release()
				cancelBody.Release()
			})
		})
	}

	buf := make([]byte, 64<<10)
	// A Read can return the last bytes and the end of the body together. The
	// bytes go out first; the end is kept for the next read() call.
	var ended error
	read = js.FuncOf(func(js.Value, []js.Value) any {
		return makePromise(func() (any, error) {
			if ended != nil {
				finish()
				if ended == io.EOF {
					return nil, nil
				}
				return nil, ended
			}
			for {
				nr, err := res.Body.Read(buf)
				if nr > 0 {
					chunk := js.Global().Get("Uint8Array").New(nr)
					js.CopyBytesToJS(chunk, buf[:nr])
					ended = err
					return chunk, nil
				}
				if err == io.EOF {
					finish()
					return nil, nil
				}
				if err != nil {
					finish()
					return nil, err
				}
			}
		})
	})
	cancelBody = js.FuncOf(func(js.Value, []js.Value) any {
		go finish()
		return nil
	})

	return map[string]any{
		"status":     res.StatusCode,
		"statusText": strings.TrimPrefix(res.Status, fmt.Sprintf("%d ", res.StatusCode)),
		"url":        res.Request.URL.String(),
		"redirected": res.Request.Response != nil,
		"headers":    headers,
		"read":       read,
		"cancel":     cancelBody,
	}
}

func call(f js.Value, args ...any) {
	if f.Type() == js.TypeFunction {
		f.Invoke(args...)
	}
}

func jsError(msg string) js.Value {
	return js.Global().Get("Error").New(msg)
}

func rejected(msg string) js.Value {
	return js.Global().Get("Promise").Call("reject", jsError(msg))
}

// makePromise runs f on a goroutine and settles a JS promise with its result.
func makePromise(f func() (any, error)) js.Value {
	var handler js.Func
	handler = js.FuncOf(func(this js.Value, args []js.Value) any {
		resolve, reject := args[0], args[1]
		go func() {
			defer handler.Release()
			res, err := f()
			if err != nil {
				reject.Invoke(jsError(err.Error()))
				return
			}
			resolve.Invoke(res)
		}()
		return nil
	})
	return js.Global().Get("Promise").New(handler)
}
