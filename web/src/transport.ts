// Request/response transport over the loopback websocket.
//
// Every request carries a monotonically increasing id (never reset, even
// across reconnects) and lives in a pending table with a ten-second timer.
// A reply resolves exactly the request it answers; an `error` reply
// rejects it with the server's code; a close or socket error rejects
// everything outstanding with `closed`. Broadcasts (`reviewChanged`,
// `diffChanged`) go to the events sink. There is no automatic reconnect
// and no replay: a write whose acknowledgement was lost is the user's
// call to repeat.

import {
  type ClientRequest,
  DecodeError,
  type DiffChangedMessage,
  type HelloMessage,
  type Response,
  type ReviewChangedMessage,
  type ServerMessage,
  decodeServerMessage,
} from "./protocol";

export type ConnectionState = "disconnected" | "connecting" | "connected";

export type TransportErrorKind = "error" | "closed" | "timeout" | "unauthorized" | "malformed";

export class TransportError extends Error {
  constructor(
    public readonly kind: TransportErrorKind,
    message: string,
    public readonly code: string | null = null,
  ) {
    super(message);
    this.name = "TransportError";
  }
}

/** The subset of WebSocket the transport uses (fakes implement it in tests). */
export interface SocketLike {
  send(data: string): void;
  close(): void;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
}

export type SocketFactory = () => SocketLike;

export interface TransportEvents {
  onBroadcast(msg: ReviewChangedMessage | DiffChangedMessage): void;
  onConnection(state: ConnectionState): void;
}

export interface TransportOptions {
  /** Request timeout in milliseconds (default 10 000). */
  timeoutMs?: number;
  /** Injectable timers for deterministic tests. */
  setTimeout?: (fn: () => void, ms: number) => unknown;
  clearTimeout?: (handle: unknown) => void;
}

/** `Omit` that distributes over a union instead of keeping only the common keys. */
type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;

/** A request as callers build it: any client message except `auth`, without the id. */
export type RequestBody = Exclude<DistributiveOmit<ClientRequest, "id">, { type: "auth" }>;

export interface TransportApi {
  readonly state: ConnectionState;
  readonly connected: boolean;
  connect(token: string): Promise<HelloMessage>;
  request(msg: RequestBody): Promise<Response>;
  close(): void;
}

interface PendingEntry {
  resolve: (msg: Response) => void;
  reject: (err: TransportError) => void;
  timer: unknown;
}

export class Transport implements TransportApi {
  private socket: SocketLike | null = null;
  private pending = new Map<number, PendingEntry>();
  private nextId = 1;
  private _state: ConnectionState = "disconnected";
  private helloWaiter: { resolve: (h: HelloMessage) => void; reject: (e: TransportError) => void } | null =
    null;
  private readonly timeoutMs: number;
  private readonly setTimer: (fn: () => void, ms: number) => unknown;
  private readonly clearTimer: (handle: unknown) => void;

  constructor(
    private readonly factory: SocketFactory,
    private readonly events: TransportEvents,
    opts: TransportOptions = {},
  ) {
    this.timeoutMs = opts.timeoutMs ?? 10_000;
    this.setTimer = opts.setTimeout ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = opts.clearTimeout ?? ((h) => clearTimeout(h as ReturnType<typeof setTimeout>));
  }

  get state(): ConnectionState {
    return this._state;
  }

  get connected(): boolean {
    return this._state === "connected";
  }

  /** Open a socket, authenticate, and resolve with the server's hello. */
  connect(token: string): Promise<HelloMessage> {
    this.close();
    const socket = this.factory();
    this.socket = socket;
    this.setState("connecting");
    return new Promise<HelloMessage>((resolve, reject) => {
      this.helloWaiter = { resolve, reject };
      socket.onopen = () => socket.send(JSON.stringify({ type: "auth", token }));
      socket.onmessage = (ev) => this.onMessage(socket, ev.data);
      socket.onclose = () => this.onClosed(socket, "connection closed");
      socket.onerror = () => this.onClosed(socket, "connection failed");
    });
  }

  request(msg: RequestBody): Promise<Response> {
    const socket = this.socket;
    if (!this.connected || !socket) {
      return Promise.reject(new TransportError("closed", "not connected"));
    }
    const id = this.nextId++;
    return new Promise<Response>((resolve, reject) => {
      const timer = this.setTimer(() => {
        if (this.pending.delete(id)) {
          reject(new TransportError("timeout", `no reply to ${msg.type} within ${this.timeoutMs} ms`));
        }
      }, this.timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      try {
        socket.send(JSON.stringify({ ...msg, id }));
      } catch (e) {
        this.settle(id, new TransportError("closed", `send failed: ${String(e)}`));
      }
    });
  }

  /** Close the socket; every pending request rejects with `closed`. */
  close(): void {
    const socket = this.socket;
    if (!socket) return;
    this.detach(socket);
    try {
      socket.close();
    } catch {
      // A socket that is already closed throws in some implementations.
    }
    this.finish("connection closed");
  }

  private setState(state: ConnectionState): void {
    if (this._state === state) return;
    this._state = state;
    this.events.onConnection(state);
  }

  private detach(socket: SocketLike): void {
    socket.onopen = null;
    socket.onmessage = null;
    socket.onclose = null;
    socket.onerror = null;
    if (this.socket === socket) this.socket = null;
  }

  /** Reject everything outstanding and report the disconnection. */
  private finish(reason: string): void {
    const err = new TransportError("closed", reason);
    for (const [id] of [...this.pending]) this.settle(id, err);
    if (this.helloWaiter) {
      const waiter = this.helloWaiter;
      this.helloWaiter = null;
      waiter.reject(err);
    }
    this.setState("disconnected");
  }

  private onClosed(socket: SocketLike, reason: string): void {
    if (this.socket !== socket) return;
    this.detach(socket);
    this.finish(reason);
  }

  private settle(id: number, err: TransportError | null, msg?: Response): void {
    const entry = this.pending.get(id);
    if (!entry) return;
    this.pending.delete(id);
    this.clearTimer(entry.timer);
    if (err) entry.reject(err);
    else if (msg) entry.resolve(msg);
  }

  private onMessage(socket: SocketLike, data: unknown): void {
    if (this.socket !== socket) return;
    let raw: unknown;
    try {
      raw = JSON.parse(String(data));
    } catch {
      return; // not JSON at all: nothing to attribute it to
    }
    let msg: ServerMessage;
    try {
      msg = decodeServerMessage(raw);
    } catch (e) {
      // A malformed reply rejects only the request it was meant for.
      const id = idOf(raw);
      if (id !== null) {
        const message = e instanceof DecodeError ? e.message : String(e);
        this.settle(id, new TransportError("malformed", message));
      } else if (this.helloWaiter && typeOf(raw) === "hello") {
        const waiter = this.helloWaiter;
        this.helloWaiter = null;
        waiter.reject(new TransportError("malformed", e instanceof Error ? e.message : String(e)));
      }
      return;
    }
    switch (msg.type) {
      case "hello": {
        const waiter = this.helloWaiter;
        this.helloWaiter = null;
        this.setState("connected");
        waiter?.resolve(msg);
        return;
      }
      case "reviewChanged":
      case "diffChanged":
        this.events.onBroadcast(msg);
        return;
      case "error": {
        if (msg.id === null) {
          if (this.helloWaiter) {
            const waiter = this.helloWaiter;
            this.helloWaiter = null;
            const kind = msg.code === "unauthorized" ? "unauthorized" : "error";
            waiter.reject(new TransportError(kind, msg.message, msg.code));
          }
          return;
        }
        this.settle(msg.id, new TransportError("error", msg.message, msg.code));
        return;
      }
      default:
        this.settle(msg.id, null, msg);
    }
  }
}

function idOf(raw: unknown): number | null {
  if (typeof raw !== "object" || raw === null) return null;
  const id = (raw as { id?: unknown }).id;
  return typeof id === "number" && Number.isInteger(id) ? id : null;
}

function typeOf(raw: unknown): string | null {
  if (typeof raw !== "object" || raw === null) return null;
  const type = (raw as { type?: unknown }).type;
  return typeof type === "string" ? type : null;
}
