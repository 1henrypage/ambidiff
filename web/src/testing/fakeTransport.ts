// A hand-driven stand-in for `TransportApi`: tests resolve or reject
// requests explicitly instead of round-tripping through a fake socket, so
// they can exercise state.ts's navigation-ownership races (B14) by
// controlling exactly which request answers first.
import type { HelloMessage, Response } from "../protocol";
import type { ConnectionState, RequestBody, TransportApi } from "../transport";
import { TransportError } from "../transport";

export interface PendingRequest {
  readonly msg: RequestBody;
  resolve(resp: Response): void;
  reject(err: TransportError): void;
}

export class FakeTransport implements TransportApi {
  state: ConnectionState = "disconnected";
  nextHello: HelloMessage | null = null;
  nextConnectError: TransportError | null = null;
  requests: PendingRequest[] = [];

  get connected(): boolean {
    return this.state === "connected";
  }

  connect(_token: string): Promise<HelloMessage> {
    if (this.nextConnectError) {
      const err = this.nextConnectError;
      this.nextConnectError = null;
      this.state = "disconnected";
      return Promise.reject(err);
    }
    this.state = "connected";
    if (!this.nextHello) return Promise.reject(new TransportError("closed", "no hello configured"));
    return Promise.resolve(this.nextHello);
  }

  request(msg: RequestBody): Promise<Response> {
    if (!this.connected) return Promise.reject(new TransportError("closed", "not connected"));
    return new Promise<Response>((resolve, reject) => {
      this.requests.push({ msg, resolve, reject });
    });
  }

  close(): void {
    this.state = "disconnected";
    const pending = this.requests.splice(0, this.requests.length);
    for (const p of pending) p.reject(new TransportError("closed", "connection closed"));
  }

  /** The oldest request still awaiting an answer, matching `type` (and `path` when given). */
  find(type: string, path?: string): PendingRequest | undefined {
    return this.requests.find(
      (r) => r.msg.type === type && (path === undefined || (r.msg as { path?: string }).path === path),
    );
  }

  /** Resolve `find(type, path)` and remove it from the pending set. */
  answer(type: string, path: string | undefined, resp: Response): void {
    const found = this.find(type, path);
    if (!found) throw new Error(`no pending ${type}${path ? ` for ${path}` : ""}`);
    this.requests.splice(this.requests.indexOf(found), 1);
    found.resolve(resp);
  }

  fail(type: string, path: string | undefined, err: TransportError): void {
    const found = this.find(type, path);
    if (!found) throw new Error(`no pending ${type}${path ? ` for ${path}` : ""}`);
    this.requests.splice(this.requests.indexOf(found), 1);
    found.reject(err);
  }
}
