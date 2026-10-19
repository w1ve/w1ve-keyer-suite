// Live port-forwarding test for the RWK Client and Station running on the SAME machine.
//
// It pairs the two apps through a relay you run locally, adds TCP and UDP forward rules to
// DIFFERENT station target ports, starts echo servers on those target ports, and checks that
// traffic sent to the Client's local ports comes back from the right Station target. It also
// edits the rules while connected and checks that the Station follows, then disables and
// re-enables a rule live.
//
// Each echo server prefixes its own port to every reply, so a reply such as "19503:PING" proves
// the traffic reached the Station target on port 19503.
//
// Usage (Node 22+, from the repo root):
//   node rwk-app/tests/live/forwarding.mjs [--relay 127.0.0.1:9000] [--client-debug 9222] [--station-debug 9223]
//
// Prerequisites:
//   1. A relay listening at --relay (for example rwk-relay with RWK_RELAY_ADDR=127.0.0.1:9000).
//   2. The Client launched with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<client-debug>
//      and the Station with the same flag for <station-debug>. Give the second app its own
//      WEBVIEW2_USER_DATA_FOLDER, because both apps share the com.w1ve.rwk identifier.
//
// The test leaves the apps running and removes the rules it created. It also runs on a warm
// session: each run disconnects and pairs again, so repeated runs exercise re-pairing.

import dgram from "node:dgram";
import net from "node:net";

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, arg, i, all) => {
    if (arg.startsWith("--")) acc.push([arg.slice(2), all[i + 1]]);
    return acc;
  }, []),
);
const RELAY = args.relay ?? "127.0.0.1:9000";
const CLIENT_DEBUG = Number(args["client-debug"] ?? 9222);
const STATION_DEBUG = Number(args["station-debug"] ?? 9223);

// Test ports. They are high and unlikely to be in use.
const CLIENT_TCP = 19401; // Client listens here for the TCP rule
const CLIENT_UDP = 19402; // Client listens here for the UDP rule
const CLIENT_OFF = 19403; // Client port of a rule that stays disabled
const STATION_TCP_A = 19501; // Station target for TCP, before the edit
const STATION_UDP_A = 19502; // Station target for UDP, before the edit
const STATION_OFF = 19505; // Station target for the disabled rule (must never be reached)
const STATION_TCP_B = 19503; // Station target for TCP, after the live edit
const STATION_UDP_B = 19504; // Station target for UDP, after the live edit

const results = [];
const check = (name, ok, detail = "") => {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitUntil(label, fn, timeoutMs = 15000, interval = 250) {
  const end = Date.now() + timeoutMs;
  let last;
  while (Date.now() < end) {
    try {
      last = await fn();
      if (last) return last;
    } catch (e) {
      last = e;
    }
    await sleep(interval);
  }
  throw new Error(`timed out waiting for ${label}`);
}

// ─── Chrome DevTools Protocol helper ───────────────────────────────────────────

async function connectPage(port) {
  const pages = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  const page = pages.find((p) => p.type === "page");
  if (!page) throw new Error(`no page on debug port ${port}; is the app running with the flag?`);
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => {
    ws.onopen = res;
    ws.onerror = rej;
  });
  let nextId = 0;
  const pending = new Map();
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  };
  const send = (method, params = {}) =>
    new Promise((resolve) => {
      const id = ++nextId;
      pending.set(id, resolve);
      ws.send(JSON.stringify({ id, method, params }));
    });
  await send("Runtime.enable");
  const evaluate = async (expression) => {
    const res = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (res.result?.exceptionDetails) {
      throw new Error(res.result.exceptionDetails.exception?.description ?? "evaluation failed");
    }
    return res.result?.result?.value;
  };
  return { port, evaluate, close: () => ws.close() };
}

// Calls a Tauri command the same way the UI does.
const invoke = (app, cmd, payload = {}) =>
  app.evaluate(
    `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(payload)})`,
  );

// Subscribes to the rwk://event stream inside the page and buffers every event.
async function installCapture(app) {
  await app.evaluate(`(() => {
    if (window.__rwkCapture) return true;
    const T = window.__TAURI_INTERNALS__;
    window.__rwkCapture = [];
    const handler = T.transformCallback((e) => { window.__rwkCapture.push(e.payload); });
    return T.invoke('plugin:event|listen', { event: 'rwk://event', target: { kind: 'Any' }, handler })
      .then(() => true);
  })()`);
}

const latestEvent = (app, kind) =>
  app.evaluate(
    `(() => { const evs = (window.__rwkCapture || []).filter((e) => e.kind === ${JSON.stringify(kind)}); return evs.length ? evs[evs.length - 1] : null; })()`,
  );

// The Client sends camelCase rule fields and the Station sends snake_case; normalise both.
const normaliseRule = (r) => ({
  ...r,
  clientPort: r.clientPort ?? r.client_port,
  stationPort: r.stationPort ?? r.station_port,
  stationTargetAddress: r.stationTargetAddress ?? r.station_target_address,
});

const latestRules = async (app) => {
  const rules = (await latestEvent(app, "forwardRules"))?.rules;
  return rules ? rules.map(normaliseRule) : null;
};

// ─── Echo servers on the Station target ports ─────────────────────────────────

const servers = [];

function startTcpEcho(port) {
  const srv = net.createServer((sock) => {
    sock.on("error", () => {});
    sock.on("data", (d) => sock.write(Buffer.concat([Buffer.from(`${port}:`), d])));
  });
  servers.push(srv);
  return new Promise((res, rej) => {
    srv.once("error", rej);
    srv.listen(port, "127.0.0.1", () => res());
  });
}

function startUdpEcho(port) {
  const sock = dgram.createSocket("udp4");
  sock.on("message", (msg, rinfo) => {
    sock.send(Buffer.concat([Buffer.from(`${port}:`), msg]), rinfo.port, rinfo.address);
  });
  servers.push(sock);
  return new Promise((res, rej) => {
    sock.once("error", rej);
    sock.bind(port, "127.0.0.1", () => res());
  });
}

// ─── Probes sent to the Client's local ports ──────────────────────────────────

// Sends `text` over TCP and resolves with the reply, or null if none arrives.
function tcpProbe(port, text, timeoutMs = 3000) {
  return new Promise((resolve) => {
    const expected = Buffer.byteLength(`${port}:${text}`);
    let got = Buffer.alloc(0);
    let settled = false;
    const sock = net.connect(port, "127.0.0.1");
    const finish = (value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      sock.destroy();
      resolve(value);
    };
    const timer = setTimeout(() => finish(got.length ? got.toString() : null), timeoutMs);
    sock.on("connect", () => sock.write(text));
    sock.on("data", (d) => {
      got = Buffer.concat([got, d]);
      if (got.length >= expected) finish(got.toString());
    });
    sock.on("error", () => finish(null));
    sock.on("close", () => finish(got.length ? got.toString() : null));
  });
}

// Sends one UDP datagram and resolves with the reply, or null if none arrives.
function udpProbe(port, text, timeoutMs = 2000) {
  return new Promise((resolve) => {
    const sock = dgram.createSocket("udp4");
    const timer = setTimeout(() => {
      sock.close();
      resolve(null);
    }, timeoutMs);
    sock.on("message", (msg) => {
      clearTimeout(timer);
      sock.close();
      resolve(msg.toString());
    });
    sock.on("error", () => {
      clearTimeout(timer);
      sock.close();
      resolve(null);
    });
    sock.send(Buffer.from(text), port, "127.0.0.1");
  });
}

// Retries a probe until it returns the expected reply. UDP is connectionless, so early
// datagrams can be dropped before the listener is up.
async function expectTcp(port, text, expected, timeoutMs = 10000) {
  return waitUntil(`TCP reply ${expected}`, async () => (await tcpProbe(port, text)) === expected, timeoutMs, 300)
    .then(() => true)
    .catch(() => false);
}

async function expectUdp(port, text, expected, timeoutMs = 10000) {
  return waitUntil(`UDP reply ${expected}`, async () => (await udpProbe(port, text)) === expected, timeoutMs, 300)
    .then(() => true)
    .catch(() => false);
}

// ─── Test body ────────────────────────────────────────────────────────────────

async function main() {
  // Preflight: the relay must accept connections before the apps can pair.
  const [relayHost, relayPort] = RELAY.split(":");
  const relayUp = await new Promise((resolve) => {
    const s = net.connect(Number(relayPort), relayHost);
    s.once("connect", () => {
      s.destroy();
      resolve(true);
    });
    s.once("error", () => resolve(false));
  });
  check("relay is reachable", relayUp, RELAY);
  if (!relayUp) throw new Error(`relay ${RELAY} is not running`);

  const client = await connectPage(CLIENT_DEBUG);
  const station = await connectPage(STATION_DEBUG);
  await installCapture(client);
  await installCapture(station);
  await invoke(client, "request_state");
  await invoke(station, "station_request_state");

  // ─── Pair the two apps through the relay ─────────────────────────────────────
  const pairingCode = (await waitUntil("station pairing code", async () => latestEvent(station, "settings"))).pairing_code;
  check("station exposes a pairing code", Boolean(pairingCode));

  // Start from a clean, unpaired state so a re-run on a warm session pairs like a fresh one.
  await invoke(client, "disconnect");
  await invoke(station, "station_disconnect");
  await waitUntil("client unpaired", () => client.evaluate("!/RTT \\d+ ms/.test(document.body.innerText)"), 15000).catch(() => {});
  await sleep(500);

  await invoke(station, "station_listen", { relay: RELAY, code: pairingCode });
  await invoke(client, "connect", { relay: RELAY, code: pairingCode });
  let paired = false;
  try {
    await waitUntil(
      "client paired (RTT shown)",
      () => client.evaluate("/RTT \\d+ ms/.test(document.body.innerText)"),
      60000,
    );
    paired = true;
  } catch {
    paired = false;
  }
  check("client pairs with station through the relay", paired);
  if (!paired) throw new Error("apps did not pair; check the relay and both logs");

  // ─── Echo servers on the Station targets ─────────────────────────────────────
  await startTcpEcho(STATION_TCP_A);
  await startUdpEcho(STATION_UDP_A);
  await startTcpEcho(STATION_TCP_B);
  await startUdpEcho(STATION_UDP_B);
  await startTcpEcho(STATION_OFF);

  // ─── Clear test rules left by an earlier run, then add fresh ones ───────────
  const TEST_CLIENT_PORTS = [CLIENT_TCP, CLIENT_UDP, CLIENT_OFF];
  const stale = ((await latestRules(client)) ?? []).filter((r) => TEST_CLIENT_PORTS.includes(r.clientPort));
  for (const rule of stale) await invoke(client, "remove_forward_rule", { id: rule.id });
  if (stale.length) await sleep(500);

  // ─── Add rules on the Client (different station target ports) ───────────────
  const addRule = (name, protocol, clientPort, stationPort) =>
    invoke(client, "add_forward_rule", {
      name,
      protocol,
      clientPort,
      stationPort,
      stationTargetAddress: "127.0.0.1",
    });
  await addRule("test-tcp", "tcp", CLIENT_TCP, STATION_TCP_A);
  await addRule("test-udp", "udp", CLIENT_UDP, STATION_UDP_A);
  await addRule("test-off", "tcp", CLIENT_OFF, STATION_OFF);

  const findRule = (rules, protocol, clientPort) =>
    rules?.find((r) => r.protocol === protocol && r.clientPort === clientPort);

  const tcpRule = await waitUntil("client lists test-tcp", async () =>
    findRule(await latestRules(client), "tcp", CLIENT_TCP),
  );
  const udpRule = await waitUntil("client lists test-udp", async () =>
    findRule(await latestRules(client), "udp", CLIENT_UDP),
  );
  const offRule = await waitUntil("client lists test-off", async () =>
    findRule(await latestRules(client), "tcp", CLIENT_OFF),
  );
  check("new rules start disabled", !tcpRule.enabled && !udpRule.enabled && !offRule.enabled);

  await invoke(client, "set_forward_rule_enabled", { id: tcpRule.id, enabled: true });
  await invoke(client, "set_forward_rule_enabled", { id: udpRule.id, enabled: true });

  await waitUntil("station receives enabled rules", async () => {
    const rules = await latestRules(station);
    return (
      findRule(rules, "tcp", CLIENT_TCP)?.enabled === true &&
      findRule(rules, "udp", CLIENT_UDP)?.enabled === true
    );
  });
  check("station table shows both rules enabled", true);

  // ─── Phase 1: forwarding works for TCP and UDP to different target ports ────
  const tcpOk = await expectTcp(CLIENT_TCP, "PING-TCP-1", `${STATION_TCP_A}:PING-TCP-1`);
  check(`TCP ${CLIENT_TCP} -> station target ${STATION_TCP_A}`, tcpOk);
  const udpOk = await expectUdp(CLIENT_UDP, "PING-UDP-1", `${STATION_UDP_A}:PING-UDP-1`);
  check(`UDP ${CLIENT_UDP} -> station target ${STATION_UDP_A}`, udpOk);

  // The disabled rule must not forward anything.
  const offReply = await tcpProbe(CLIENT_OFF, "PING-OFF", 3000);
  check(`disabled rule ${CLIENT_OFF} forwards nothing`, offReply === null, offReply ? `got "${offReply}"` : "");

  // ─── Phase 2: edit the rules live, while connected ───────────────────────────
  const edit = (rule, name, stationPort) =>
    invoke(client, "update_forward_rule", {
      id: rule.id,
      name,
      protocol: rule.protocol,
      clientPort: rule.clientPort,
      stationPort,
      stationTargetAddress: "127.0.0.1",
    });
  await edit(tcpRule, "test-tcp", STATION_TCP_B);
  await edit(udpRule, "test-udp", STATION_UDP_B);

  const stationFollowed = await waitUntil("station follows the edit", async () => {
    const rules = await latestRules(station);
    return (
      findRule(rules, "tcp", CLIENT_TCP)?.stationPort === STATION_TCP_B &&
      findRule(rules, "udp", CLIENT_UDP)?.stationPort === STATION_UDP_B
    );
  }).then(() => true).catch(() => false);
  check("station table updates to the edited target ports", stationFollowed);

  const tcpMoved = await expectTcp(CLIENT_TCP, "PING-TCP-2", `${STATION_TCP_B}:PING-TCP-2`);
  check(`TCP ${CLIENT_TCP} now reaches station target ${STATION_TCP_B}`, tcpMoved);
  const udpMoved = await expectUdp(CLIENT_UDP, "PING-UDP-2", `${STATION_UDP_B}:PING-UDP-2`);
  check(`UDP ${CLIENT_UDP} now reaches station target ${STATION_UDP_B}`, udpMoved);

  // ─── Phase 3: disable and re-enable TCP live ─────────────────────────────────
  await invoke(client, "set_forward_rule_enabled", { id: tcpRule.id, enabled: false });
  await waitUntil("station sees TCP rule disabled", async () =>
    findRule(await latestRules(station), "tcp", CLIENT_TCP)?.enabled === false,
  ).catch(() => {});
  const whileOff = await tcpProbe(CLIENT_TCP, "PING-TCP-OFF", 3000);
  check(`TCP ${CLIENT_TCP} stops forwarding when disabled`, whileOff === null, whileOff ? `got "${whileOff}"` : "");

  await invoke(client, "set_forward_rule_enabled", { id: tcpRule.id, enabled: true });
  const back = await expectTcp(CLIENT_TCP, "PING-TCP-3", `${STATION_TCP_B}:PING-TCP-3`);
  check(`TCP ${CLIENT_TCP} resumes after re-enable`, back);

  // ─── Cleanup: remove the test rules and stop the echo servers ───────────────
  for (const rule of [tcpRule, udpRule, offRule]) {
    await invoke(client, "remove_forward_rule", { id: rule.id });
  }
  for (const s of servers) s.close();
  client.close();
  station.close();

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  process.exit(failed.length ? 1 : 0);
}

main().catch((err) => {
  console.error(`ERROR  ${err.message}`);
  for (const s of servers) {
    try {
      s.close();
    } catch {
      // best effort: the server may already be closed
    }
  }
  process.exit(1);
});
