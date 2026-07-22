const OVERALL_STATES = new Set(["ready", "active", "degraded", "unavailable"]);
const GRANT_LABELS = {
  view: "View",
  pointer_keyboard: "Pointer + keyboard",
  gamepad: "Gamepad",
};

function record(value) {
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

function text(value, fallback = "") {
  return typeof value === "string" && value.length > 0 ? value : fallback;
}

function boolean(value) {
  return value === true;
}

function integer(value, fallback = 0) {
  return Number.isSafeInteger(value) ? value : fallback;
}

export function unwrapRpc(envelope) {
  const response = record(envelope);
  if (response.ok === true && Object.hasOwn(response, "value")) {
    return response.value;
  }
  const message = text(response.error, "Sigil management request failed");
  throw new Error(message);
}

export function formatUptime(milliseconds) {
  if (!Number.isFinite(milliseconds) || milliseconds < 0) return "Unavailable";
  const totalMinutes = Math.floor(milliseconds / 60_000);
  const days = Math.floor(totalMinutes / 1_440);
  const hours = Math.floor((totalMinutes % 1_440) / 60);
  const minutes = totalMinutes % 60;
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

export function normalizeSnapshot(rawValue) {
  const root = record(rawValue);
  const service = record(root.service);
  const appliance = record(root.appliance ?? root.status ?? root.sigil ?? rawValue);
  const identity = record(appliance.identity);
  const enrollment = record(appliance.enrollment);
  const runtime = record(appliance.runtime);
  const capabilities = record(root.capabilities);

  const installed = root.installed === false ? false : service.installed !== false;
  const compatible = root.compatible !== false;
  const serviceState = text(
    service.state ?? service.active_state,
    boolean(service.active) ? "active" : "unknown",
  );
  const serviceActive = boolean(service.active) || serviceState === "active";
  const serviceSubState = text(service.sub_state);
  const overallCandidate = text(appliance.overall, "unavailable");
  const overall = OVERALL_STATES.has(overallCandidate) ? overallCandidate : "unavailable";
  const peerFingerprint = text(enrollment.peer_fingerprint) || null;
  const grants = Array.isArray(enrollment.grants)
    ? enrollment.grants
        .filter((grant) => typeof grant === "string")
        .map((grant) => GRANT_LABELS[grant] ?? grant)
    : [];

  let summary = "Unavailable";
  if (!installed) summary = "Not installed";
  else if (!compatible) summary = "Upgrade required";
  else if (overall === "active") summary = "Streaming";
  else if (overall === "ready" && serviceActive) summary = "Ready";
  else if (overall === "degraded") summary = "Degraded";
  else if (serviceActive) summary = "Starting";
  else if (serviceState === "failed") summary = "Failed";
  else summary = "Stopped";

  const lastError = record(runtime.last_error);
  return {
    installed,
    compatible,
    serviceActive,
    serviceState,
    serviceLabel: serviceSubState ? `${serviceState} (${serviceSubState})` : serviceState,
    summary,
    overall,
    version: text(appliance.sigil_version, "Unknown"),
    hostFingerprint: text(identity.host_fingerprint, "Unavailable"),
    uptime: formatUptime(runtime.uptime_ms),
    sessionActive: runtime.session === "active",
    lastError: text(lastError.code) || null,
    managementError: text(root.error) || null,
    peerFingerprint,
    grants,
    epoch: integer(enrollment.epoch),
    pendingTransaction: record(appliance.config).pending_transaction ?? null,
    streamDiagnosticsAvailable: capabilities.stream_diagnostics === true,
    factoryResetAvailable: capabilities.factory_reset === true,
  };
}

export function normalizeConfig(rawValue) {
  const value = record(rawValue);
  const settings = record(value.settings);
  const resolution = record(settings.resolution);
  const rateControl = settings.rate_control === null ? null : record(settings.rate_control);
  const resolutionMode = resolution.mode === "fixed" ? "fixed" : "native";
  const rateMode = rateControl === null ? "unavailable" : rateControl.mode === "cqp" ? "cqp" : "cbr";
  return {
    revision: text(value.revision),
    pendingTransaction: value.pending_transaction ?? null,
    draft: {
      resolutionMode,
      width: String(integer(resolution.width, 1280)),
      height: String(integer(resolution.height, 800)),
      framerate: String(integer(settings.framerate, 60)),
      rateMode,
      bitrateKbps: String(integer(rateControl?.bitrate_kbps, 12_000)),
      quantizer: String(integer(rateControl?.quantizer, 24)),
    },
  };
}

function boundedInteger(value, label, minimum, maximum, requireEven = false) {
  if (!/^[0-9]+$/.test(value)) throw new Error(`${label} must be a whole number`);
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < minimum || parsed > maximum) {
    throw new Error(`${label} must be between ${minimum} and ${maximum}`);
  }
  if (requireEven && parsed % 2 !== 0) throw new Error(`${label} must be even`);
  return parsed;
}

export function buildConfigRequest(revision, draft) {
  if (!/^sha256:[0-9a-f]{64}$/.test(revision)) {
    throw new Error("Reload configuration before applying changes");
  }
  const framerate = boundedInteger(draft.framerate, "Frame rate", 1, 240);
  const resolution =
    draft.resolutionMode === "native"
      ? { mode: "native" }
      : {
          mode: "fixed",
          width: boundedInteger(draft.width, "Width", 64, 7680, true),
          height: boundedInteger(draft.height, "Height", 64, 4320, true),
        };

  let rateControl = null;
  if (draft.rateMode === "cbr") {
    rateControl = {
      mode: "cbr",
      bitrate_kbps: boundedInteger(draft.bitrateKbps, "Bitrate", 1000, 100000),
    };
  } else if (draft.rateMode === "cqp") {
    rateControl = {
      mode: "cqp",
      quantizer: boundedInteger(draft.quantizer, "Quantizer", 1, 51),
    };
  } else if (draft.rateMode !== "unavailable") {
    throw new Error("Choose CBR or CQP rate control");
  }

  return {
    schema_version: 1,
    expected_revision: revision,
    settings: { resolution, framerate, rate_control: rateControl },
  };
}

export function transactionId(pending) {
  return text(record(pending).transaction) || null;
}
