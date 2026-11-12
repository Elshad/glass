export interface TailscaleClientOptions {
  /**
   * The only destinations this client will connect to, as "host:port".
   * host is an IP address, a CIDR range, a host name or "*.suffix";
   * port is a number or "*". IPv6 goes in brackets.
   *
   *   ["100.80.10.20:3000", "api.tail1234.ts.net:443", "100.64.0.0/10:8080"]
   *
   * Fixed for the life of the client and enforced inside the WebAssembly
   * module on every connection, redirects included.
   */
  allow: string[];

  /**
   * Returns a Tailscale auth key for this visitor. Called on every connect().
   * Have your own backend mint it after it has authenticated the visitor:
   * ephemeral, single-use, short-lived, pre-approved and tagged.
   */
  getAuthKey?: () => string | Promise<string>;

  /** A fixed auth key. For local testing only: anyone who loads the page can read it. */
  authKey?: string;

  /**
   * Sign in interactively instead of with a key: called with a
   * login.tailscale.com URL to open for the person using the page.
   */
  onLoginURL?: (url: string) => void;

  /** The node's name on the tailnet. Tailscale adds a suffix if it is taken. */
  hostname?: string;
  /** A control server other than Tailscale's, such as Headscale. */
  controlURL?: string;

  /** Where tsfetch.wasm is. Default: dist/tsfetch.wasm beside tsfetch.js. */
  wasmURL?: string;
  /** Where Go's wasm_exec.js is. Default: dist/wasm_exec.js beside tsfetch.js. */
  wasmExecURL?: string;

  /** How long connect() waits to join the tailnet. Default 30000. */
  connectTimeoutMs?: number;
  /** How long connect() waits for an interactive sign-in. Default 300000. */
  loginTimeoutMs?: number;

  onStateChange?: (state: TailscaleState) => void;
  /** Log Tailscale's internals to the console. */
  debug?: boolean;
}

export type TailscaleState = "disconnected" | "connecting" | "connected";

export declare class TailscaleError extends Error {
  code: "config" | "auth" | "timeout" | "closed" | "blocked";
}

export declare class TailscaleClient {
  constructor(options: TailscaleClientOptions);

  readonly state: TailscaleState;
  /** This node's tailnet addresses, once connected. */
  readonly addresses: string[];
  /** This node's MagicDNS name, once connected. */
  readonly name: string;

  connect(): Promise<void>;
  disconnect(): Promise<void>;

  /**
   * window.fetch through the tailnet node. Honours method, headers, body,
   * signal and redirect. There are no cookies, no cache and no CORS; a
   * streamed request body is read to the end before sending.
   */
  fetch(url: string | URL, options?: RequestInit): Promise<Response>;
}

export declare function createTailscaleClient(options: TailscaleClientOptions): TailscaleClient;
