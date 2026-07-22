import assert from "node:assert/strict";
import test from "node:test";

import {
  buildConfigRequest,
  formatUptime,
  isValidHostFingerprint,
  normalizeConfig,
  normalizeSnapshot,
  transactionId,
  unwrapRpc,
} from "../src/model.mjs";

const revision = `sha256:${"a".repeat(64)}`;

test("unwrapRpc accepts only a successful envelope", () => {
  assert.deepEqual(unwrapRpc({ ok: true, value: { ready: true } }), { ready: true });
  assert.throws(() => unwrapRpc({ ok: false, error: "service timeout" }), /service timeout/);
  assert.throws(() => unwrapRpc({ ok: true }), /request failed/);
});

test("normalizes the backend snapshot without exposing endpoint identity", () => {
  const snapshot = normalizeSnapshot({
    schema_version: 1,
    installed: true,
    compatible: true,
    service: { installed: true, active: true, active_state: "active", sub_state: "running" },
    capabilities: { stream_diagnostics: false, factory_reset: false },
    appliance: {
      sigil_version: "0.4.0",
      overall: "active",
      identity: { host_fingerprint: "12345678…90abcdef" },
      enrollment: {
        peer_fingerprint: "abcdef12…34567890",
        grants: ["view", "pointer_keyboard", "gamepad"],
        epoch: 7,
      },
      config: { pending_transaction: { transaction: "tx-1" } },
      runtime: { uptime_ms: 3_720_000, session: "active", last_error: null },
    },
  });

  assert.equal(snapshot.summary, "Streaming");
  assert.equal(snapshot.serviceLabel, "active (running)");
  assert.equal(snapshot.uptime, "1h 2m");
  assert.equal(snapshot.peerFingerprint, "abcdef12…34567890");
  assert.deepEqual(snapshot.grants, ["View", "Pointer + keyboard", "Gamepad"]);
  assert.equal(snapshot.streamDiagnosticsAvailable, false);
  assert.equal(snapshot.factoryResetAvailable, false);
  assert.equal(transactionId(snapshot.pendingTransaction), "tx-1");
});

test("normalizes native CBR configuration and builds its strict request", () => {
  const config = normalizeConfig({
    schema_version: 1,
    revision,
    settings: {
      resolution: { mode: "native" },
      framerate: 60,
      rate_control: { mode: "cbr", bitrate_kbps: 12000 },
    },
    pending_transaction: null,
  });

  assert.equal(config.draft.resolutionMode, "native");
  assert.equal(config.draft.rateMode, "cbr");
  assert.deepEqual(buildConfigRequest(config.revision, config.draft), {
    schema_version: 1,
    expected_revision: revision,
    settings: {
      resolution: { mode: "native" },
      framerate: 60,
      rate_control: { mode: "cbr", bitrate_kbps: 12000 },
    },
  });
});

test("builds fixed CQP configuration without pinning a product resolution", () => {
  const request = buildConfigRequest(revision, {
    resolutionMode: "fixed",
    width: "2560",
    height: "1600",
    framerate: "144",
    rateMode: "cqp",
    bitrateKbps: "12000",
    quantizer: "24",
  });
  assert.deepEqual(request.settings.resolution, { mode: "fixed", width: 2560, height: 1600 });
  assert.deepEqual(request.settings.rate_control, { mode: "cqp", quantizer: 24 });
});

test("rejects unsafe or invalid local edits before backend validation", () => {
  const base = {
    resolutionMode: "fixed",
    width: "1281",
    height: "800",
    framerate: "60",
    rateMode: "cbr",
    bitrateKbps: "12000",
    quantizer: "24",
  };
  assert.throws(() => buildConfigRequest(revision, base), /Width must be even/);
  assert.throws(() => buildConfigRequest("not-a-revision", { ...base, width: "1280" }), /Reload/);
  assert.throws(
    () => buildConfigRequest(revision, { ...base, width: "1280", framerate: "241" }),
    /Frame rate must be between 1 and 240/,
  );
});

test("formats short and absent uptime safely", () => {
  assert.equal(formatUptime(59_999), "0m");
  assert.equal(formatUptime(undefined), "Unavailable");
});

test("accepts only the redacted lowercase host fingerprint interlock", () => {
  assert.equal(isValidHostFingerprint("12345678…90abcdef"), true);
  assert.equal(isValidHostFingerprint("12345678...90abcdef"), false);
  assert.equal(isValidHostFingerprint("12345678…90abcdeF"), false);
  assert.equal(isValidHostFingerprint("Unavailable"), false);
  assert.equal(isValidHostFingerprint(undefined), false);
});
