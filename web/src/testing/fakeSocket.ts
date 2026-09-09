// A scriptable stand-in for WebSocket: tests open it, inspect what the page
// sent, and push server frames or close events by hand.
import type { SocketLike } from "../transport";

export class FakeSocket implements SocketLike {
  sent: string[] = [];
  closed = false;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;

  send(data: string): void {
    if (this.closed) throw new Error("socket is closed");
    this.sent.push(data);
  }

  close(): void {
    this.closed = true;
  }

  /** The server side: open the socket. */
  open(): void {
    this.onopen?.({});
  }

  /** The server side: deliver a frame (objects are JSON-encoded). */
  receive(frame: unknown): void {
    this.onmessage?.({ data: typeof frame === "string" ? frame : JSON.stringify(frame) });
  }

  /** The server side: drop the connection. */
  drop(): void {
    this.closed = true;
    this.onclose?.({});
  }

  /** Parsed copies of everything the page sent. */
  sentJson(): Record<string, unknown>[] {
    return this.sent.map((s) => JSON.parse(s) as Record<string, unknown>);
  }
}

/** Deterministic timers: `fire()` runs every scheduled callback. */
export class FakeTimers {
  private scheduled = new Map<number, () => void>();
  private next = 1;

  setTimeout = (fn: () => void, _ms: number): unknown => {
    const handle = this.next++;
    this.scheduled.set(handle, fn);
    return handle;
  };

  clearTimeout = (handle: unknown): void => {
    this.scheduled.delete(handle as number);
  };

  pendingCount(): number {
    return this.scheduled.size;
  }

  fire(): void {
    const callbacks = [...this.scheduled.values()];
    this.scheduled.clear();
    for (const cb of callbacks) cb();
  }
}
