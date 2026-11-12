// tsfetch: HTTP requests from a browser tab to services on a tailnet.
//
// The tab becomes an ephemeral Tailscale node (Tailscale compiled to
// WebAssembly, with its own WireGuard and TCP/IP stack), and fetch() sends the
// request through that stack. Nothing is installed on the visitor's computer.
//
//   const tailscale = createTailscaleClient({
//     allow: ["100.80.10.20:3000"],
//     getAuthKey: () => fetch("/api/tailnet-key", { method: "POST" }).then((r) => r.text()),
//   });
//   await tailscale.connect();
//   const res = await tailscale.fetch("http://100.80.10.20:3000/api/status");
//
// See README.md for what this does and does not protect.

const here = new URL(".", import.meta.url);

/** Compiled modules, by URL, so reconnecting doesn't download 5 MB again. */
const modules = new Map();
let goRuntime;
let handshakes = 0;

// Statuses whose Response must not carry a body.
const NULL_BODY = new Set([101, 103, 204, 205, 304]);

export class TailscaleError extends Error {
  /**
   * @param {string} message
   * @param {"config"|"auth"|"timeout"|"closed"|"blocked"} code
   */
  constructor(message, code) {
    super(message);
    this.name = "TailscaleError";
    this.code = code;
  }
}

export class TailscaleClient {
  #opts;
  #state = "disconnected";
  #api = null;
  #connecting = null;
  #self = { name: "", addresses: [] };
  #lastError = "";
  #live = new Set();
  #abortConnect = null;

  /** @param {import("./tsfetch").TailscaleClientOptions} options */
  constructor(options) {
    const o = { ...options };
    if (!Array.isArray(o.allow) || o.allow.length === 0) {
      throw new TailscaleError(
        'allow is required: list the destinations this page may reach, like "100.80.10.20:3000"',
        "config",
      );
    }
    const ways = [o.authKey, o.getAuthKey, o.onLoginURL].filter(Boolean).length;
    if (ways === 0) {
      throw new TailscaleError("give one of getAuthKey, authKey or onLoginURL", "config");
    }
    o.allow = o.allow.map(String);
    this.#opts = o;
  }

  /** "disconnected" | "connecting" | "connected" */
  get state() {
    return this.#state;
  }

  /** This node's tailnet addresses, once connected. */
  get addresses() {
    return [...this.#self.addresses];
  }

  /** This node's MagicDNS name, once connected. */
  get name() {
    return this.#self.name;
  }

  /** Joins the tailnet. Resolves once requests can be sent. */
  connect() {
    if (this.#state === "connected") return Promise.resolve();
    if (!this.#connecting) {
      this.#connecting = this.#connect().finally(() => {
        this.#connecting = null;
      });
    }
    return this.#connecting;
  }

  async #connect() {
    const o = this.#opts;
    this.#setState("connecting");
    let api = null;
    try {
      // The key is asked for at connect time and never kept on the client
      // object: a single-use key is spent the moment the node registers.
      let authKey = o.authKey;
      if (o.getAuthKey) authKey = await o.getAuthKey();
      if (!authKey && !o.onLoginURL) throw new TailscaleError("getAuthKey returned nothing", "auth");

      api = await startModule(o);
      this.#api = api;

      const timeoutMs = o.connectTimeoutMs ?? 30000;
      await new Promise((resolve, reject) => {
        this.#abortConnect = () => reject(new TailscaleError("disconnect() was called", "closed"));
        let timer = setTimeout(
          () => reject(new TailscaleError(`not connected after ${timeoutMs} ms` + this.#why(), "timeout")),
          timeoutMs,
        );
        api.exited.then(() => reject(new TailscaleError("the Tailscale module stopped", "closed")));
        api.bridge
          .start({
            authKey: authKey || undefined,
            interactive: !authKey,
            controlURL: o.controlURL,
            hostname: o.hostname || "tsfetch",
            allow: o.allow,
            debug: !!o.debug,
            onState: (s) => {
              if (s === "Running") {
                clearTimeout(timer);
                resolve();
              } else if (s === "NeedsMachineAuth") {
                this.#lastError = "the node is waiting for an admin to approve it";
              }
            },
            onLoginURL: (url) => {
              if (authKey) {
                // Control only asks for a sign-in when the key didn't work.
                clearTimeout(timer);
                reject(new TailscaleError("the tailnet did not accept the auth key" + this.#why(), "auth"));
                return;
              }
              // A person is signing in: give them time.
              clearTimeout(timer);
              timer = setTimeout(
                () => reject(new TailscaleError("sign-in was not finished in time", "timeout")),
                o.loginTimeoutMs ?? 300000,
              );
              o.onLoginURL(url);
            },
            onError: (msg) => {
              this.#lastError = String(msg);
              if (authKey && /\bkey\b/i.test(this.#lastError)) {
                clearTimeout(timer);
                reject(new TailscaleError("the tailnet did not accept the auth key" + this.#why(), "auth"));
              }
            },
          })
          .catch((e) => reject(new TailscaleError(messageOf(e), "config")));
      });

      try {
        this.#self = JSON.parse(api.bridge.self());
      } catch {
        this.#self = { name: "", addresses: [] };
      }
      api.exited.then(() => {
        if (this.#api === api) this.#dropped();
      });
      this.#setState("connected");
    } catch (e) {
      this.#api = null;
      if (api) api.bridge.stop().catch(() => {});
      this.#setState("disconnected");
      throw e;
    } finally {
      this.#abortConnect = null;
    }
  }

  /** Leaves the tailnet. The ephemeral node is removed from it at once. */
  async disconnect() {
    if (this.#connecting) {
      this.#abortConnect?.();
      await this.#connecting.catch(() => {});
    }
    const api = this.#api;
    if (!api) return;
    this.#dropped();
    await Promise.race([api.bridge.stop().catch(() => {}), api.exited, sleep(6000)]);
  }

  /**
   * Like window.fetch, but the request leaves through the tailnet node.
   * @param {string | URL} url
   * @param {RequestInit} [init]
   * @returns {Promise<Response>}
   */
  async fetch(url, init = {}) {
    const api = this.#api;
    if (this.#state !== "connected" || !api) {
      throw new TailscaleError("not connected: call connect() first", "closed");
    }
    const signal = init.signal ?? null;
    signal?.throwIfAborted();

    const target = new URL(String(url));
    if (target.protocol !== "http:" && target.protocol !== "https:") {
      throw new TypeError(`tsfetch: only http and https URLs, not ${target.protocol}`);
    }
    target.hash = "";

    const method = normalizeMethod(init.method);
    const headers = new Headers(init.headers ?? undefined);
    let body;
    if (init.body != null) {
      if (method === "GET" || method === "HEAD") {
        throw new TypeError("tsfetch: a GET or HEAD request can't have a body");
      }
      // Let the platform serialize strings, FormData, Blobs and the rest, and
      // tell us the Content-Type it would have used.
      const probe = new Request("http://tsfetch.invalid/", { method: "POST", body: init.body, duplex: "half" });
      body = new Uint8Array(await probe.arrayBuffer());
      const type = probe.headers.get("content-type");
      if (type && !headers.has("content-type")) headers.set("content-type", type);
      signal?.throwIfAborted();
    }

    const call = api.bridge.fetch({
      url: target.href,
      method,
      headers: [...headers],
      body,
      redirect: init.redirect ?? "follow",
    });
    const entry = { abort: call.abort };
    this.#live.add(entry);
    const settle = () => {
      this.#live.delete(entry);
      signal?.removeEventListener("abort", onAbort);
    };
    const onAbort = () => {
      if (this.#live.has(entry)) call.abort();
    };
    signal?.addEventListener("abort", onAbort, { once: true });

    let raw;
    try {
      raw = await call.promise;
    } catch (e) {
      settle();
      if (signal?.aborted) throw signal.reason;
      const msg = messageOf(e);
      if (msg.includes("destination not allowed")) {
        throw new TailscaleError(`tsfetch: ${msg} (not in this client's allow list)`, "blocked");
      }
      throw new TypeError(`tsfetch: ${msg}`);
    }

    let stream = null;
    if (NULL_BODY.has(raw.status) || method === "HEAD") {
      raw.cancel();
      settle();
    } else {
      stream = new ReadableStream({
        pull: async (controller) => {
          try {
            const chunk = await raw.read();
            if (chunk) return controller.enqueue(chunk);
            settle();
            controller.close();
          } catch (e) {
            settle();
            controller.error(signal?.aborted ? signal.reason : new TypeError(`tsfetch: ${messageOf(e)}`));
          }
        },
        cancel: () => {
          if (this.#live.has(entry)) raw.cancel();
          settle();
        },
      });
    }

    const response = new Response(stream, {
      status: raw.status,
      statusText: raw.statusText,
      headers: raw.headers,
    });
    Object.defineProperty(response, "url", { value: raw.url });
    Object.defineProperty(response, "redirected", { value: !!raw.redirected });
    return response;
  }

  #dropped() {
    this.#api = null;
    this.#live.clear();
    this.#self = { name: "", addresses: [] };
    this.#setState("disconnected");
  }

  #why() {
    return this.#lastError ? `: ${this.#lastError}` : "";
  }

  #setState(s) {
    if (this.#state === s) return;
    this.#state = s;
    try {
      this.#opts.onStateChange?.(s);
    } catch (e) {
      console.error(e);
    }
  }
}

/** @param {import("./tsfetch").TailscaleClientOptions} options */
export function createTailscaleClient(options) {
  return new TailscaleClient(options);
}

/** Starts one instance of the WebAssembly module and returns its bridge. */
async function startModule(o) {
  const wasmURL = new URL(o.wasmURL ?? "dist/tsfetch.wasm", here).href;
  const execURL = new URL(o.wasmExecURL ?? "dist/wasm_exec.js", here).href;

  if (!globalThis.Go) {
    // Go's loader is a classic script that defines globalThis.Go.
    goRuntime ??= import(/* @vite-ignore */ execURL);
    await goRuntime;
    if (!globalThis.Go) throw new TailscaleError(`${execURL} did not define Go`, "config");
  }
  if (!modules.has(wasmURL)) {
    const compiled = compile(wasmURL);
    compiled.catch(() => modules.delete(wasmURL));
    modules.set(wasmURL, compiled);
  }
  const module = await modules.get(wasmURL);

  const go = new globalThis.Go();
  const name = `__tsfetch_${++handshakes}_${Math.random().toString(36).slice(2)}`;
  const bridge = new Promise((resolve) => {
    globalThis[name] = (b) => {
      delete globalThis[name];
      resolve(b);
    };
  });
  go.argv = ["tsfetch", name];
  const instance = await WebAssembly.instantiate(module, go.importObject);
  const exited = go.run(instance).then(
    () => {},
    () => {},
  );
  const first = await Promise.race([bridge, exited.then(() => null)]);
  if (!first) {
    delete globalThis[name];
    throw new TailscaleError("the Tailscale module exited while starting", "closed");
  }
  return { bridge: first, exited };
}

async function compile(url) {
  const res = await fetch(url);
  if (!res.ok) throw new TailscaleError(`could not load ${url}: HTTP ${res.status}`, "config");
  if ((res.headers.get("content-type") || "").startsWith("application/wasm")) {
    return WebAssembly.compileStreaming(res);
  }
  return WebAssembly.compile(await res.arrayBuffer());
}

function normalizeMethod(m) {
  const method = String(m ?? "GET");
  const upper = method.toUpperCase();
  if (!/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(method)) throw new TypeError(`tsfetch: bad method ${method}`);
  return ["GET", "HEAD", "POST", "PUT", "DELETE", "OPTIONS", "PATCH"].includes(upper) ? upper : method;
}

function messageOf(e) {
  return e instanceof Error ? e.message : String(e);
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}
