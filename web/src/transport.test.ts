import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { Transport, TransportError, type ConnectionState } from "./transport";
import { FakeSocket, FakeTimers } from "./testing/fakeSocket";

const DIR = join(import.meta.dirname, "../../fixtures/contracts");
const hello = () => JSON.parse(readFileSync(join(DIR, "web-hello.json"), "utf8")) as Record<string, unknown>;

function harness() {
  const sockets: FakeSocket[] = [];
  const timers = new FakeTimers();
  const broadcasts: unknown[] = [];
  const states: ConnectionState[] = [];
  const transport = new Transport(
    () => {
      const s = new FakeSocket();
      sockets.push(s);
      return s;
    },
    {
      onBroadcast: (m) => broadcasts.push(m),
      onConnection: (s) => states.push(s),
    },
    { timeoutMs: 10_000, setTimeout: timers.setTimeout, clearTimeout: timers.clearTimeout },
  );
  return { transport, sockets, timers, broadcasts, states };
}

async function connected() {
  const h = harness();
  const pending = h.transport.connect("tok");
  const socket = h.sockets[0]!;
  socket.open();
  expect(socket.sentJson()[0]).toEqual({ type: "auth", token: "tok" });
  socket.receive(hello());
  await pending;
  return { ...h, socket };
}

async function rejection(p: Promise<unknown>): Promise<TransportError> {
  try {
    await p;
  } catch (e) {
    expect(e).toBeInstanceOf(TransportError);
    return e as TransportError;
  }
  throw new Error("expected a rejection");
}

describe("transport", () => {
  test("connect_resolves_on_hello_and_rejects_on_unauthorized", async () => {
    const h = harness();
    const p = h.transport.connect("bad");
    h.sockets[0]!.open();
    h.sockets[0]!.receive({ type: "error", id: null, code: "unauthorized", message: "unauthorized" });
    const err = await rejection(p);
    expect(err.kind).toBe("unauthorized");
    expect(h.transport.connected).toBe(false);

    const ok = await connected();
    expect(ok.transport.state).toBe("connected");
    expect(ok.states).toEqual(["connecting", "connected"]);
  });

  test("ids_are_monotonic_and_echoed", async () => {
    const h = await connected();
    const a = h.transport.request({ type: "getFile", path: "a" });
    const b = h.transport.request({ type: "getFile", path: "b" });
    const sent = h.socket.sentJson();
    expect(sent[1]!["id"]).toBe(1);
    expect(sent[2]!["id"]).toBe(2);
    h.socket.receive({ type: "src", id: 2, path: "b", content: "B" });
    h.socket.receive({ type: "src", id: 1, path: "a", content: "A" });
    expect((await a).type).toBe("src");
    expect((await b).type).toBe("src");
    // Reconnecting never reuses an id.
    h.transport.close();
    const again = h.transport.connect("tok");
    h.sockets[1]!.open();
    h.sockets[1]!.receive(hello());
    await again;
    void h.transport.request({ type: "refresh" });
    expect(h.sockets[1]!.sentJson()[1]!["id"]).toBe(3);
  });

  test("same_path_double_request_both_resolve", async () => {
    const h = await connected();
    const first = h.transport.request({ type: "getFile", path: "same" });
    const second = h.transport.request({ type: "getFile", path: "same" });
    h.socket.receive({ type: "src", id: 1, path: "same", content: "1" });
    h.socket.receive({ type: "src", id: 2, path: "same", content: "2" });
    const [a, b] = await Promise.all([first, second]);
    expect(a.type === "src" && a.content).toBe("1");
    expect(b.type === "src" && b.content).toBe("2");
  });

  test("error_response_rejects_and_clears_pending", async () => {
    const h = await connected();
    const p = h.transport.request({ type: "comment.add", body: "x" });
    h.socket.receive({ type: "error", id: 1, code: "decode", message: "bad" });
    const err = await rejection(p);
    expect(err.kind).toBe("error");
    expect(err.code).toBe("decode");
    expect(err.message).toBe("bad");
    expect(h.timers.pendingCount()).toBe(0);
    // A late duplicate reply for the same id is ignored.
    h.socket.receive({ type: "comment", id: 1, comment: {}, warnings: [] });
  });

  test("timeout_rejects", async () => {
    const h = await connected();
    const p = h.transport.request({ type: "refresh" });
    h.timers.fire();
    const err = await rejection(p);
    expect(err.kind).toBe("timeout");
    // A reply after the timeout is dropped, not delivered to anyone.
    h.socket.receive({ type: "snapshot", id: 1, ...hello() });
  });

  test("close_rejects_all", async () => {
    const h = await connected();
    const a = h.transport.request({ type: "refresh" });
    const b = h.transport.request({ type: "getSrc", path: "p" });
    h.socket.drop();
    expect((await rejection(a)).kind).toBe("closed");
    expect((await rejection(b)).kind).toBe("closed");
    expect(h.transport.connected).toBe(false);
    expect(h.states.at(-1)).toBe("disconnected");
    expect(h.timers.pendingCount()).toBe(0);
    const late = await rejection(h.transport.request({ type: "refresh" }));
    expect(late.kind).toBe("closed");
  });

  test("malformed_frame_rejects_only_its_id", async () => {
    const h = await connected();
    const bad = h.transport.request({ type: "getFile", path: "a" });
    const good = h.transport.request({ type: "getFile", path: "b" });
    h.socket.receive({ type: "file", id: 1, path: "a" }); // missing entry
    h.socket.receive("not json at all");
    h.socket.receive({ type: "src", id: 2, path: "b", content: "ok" });
    expect((await rejection(bad)).kind).toBe("malformed");
    expect((await good).type).toBe("src");
  });

  test("broadcasts_reach_the_sink_and_stale_socket_events_are_ignored", async () => {
    const h = await connected();
    h.socket.receive({
      type: "reviewChanged",
      review: "{}",
      warnings: [],
      readOnly: false,
      readOnlyReason: null,
      generation: 2,
    });
    expect(h.broadcasts).toHaveLength(1);
    const old = h.socket;
    h.transport.close();
    old.receive({ type: "diffChanged", files: [], skipped: [], sourceError: null, comparison: null, generation: 3 });
    expect(h.broadcasts).toHaveLength(1);
  });
});
