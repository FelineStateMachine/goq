export interface RpcEnvelope<T> {
  ok: boolean;
  value?: T;
  error?: string;
}

export interface ConfigDraft {
  resolutionMode: "native" | "fixed";
  width: string;
  height: string;
  framerate: string;
  rateMode: "cbr" | "cqp" | "unavailable";
  bitrateKbps: string;
  quantizer: string;
}

export interface ConfigRequest {
  schema_version: 1;
  expected_revision: string;
  settings: {
    resolution: { mode: "native" } | { mode: "fixed"; width: number; height: number };
    framerate: number;
    rate_control: null | { mode: "cbr"; bitrate_kbps: number } | { mode: "cqp"; quantizer: number };
  };
}

export interface SnapshotView {
  installed: boolean;
  compatible: boolean;
  serviceActive: boolean;
  serviceState: string;
  serviceLabel: string;
  summary: string;
  overall: string;
  version: string;
  hostFingerprint: string;
  uptime: string;
  sessionActive: boolean;
  lastError: string | null;
  managementError: string | null;
  peerFingerprint: string | null;
  grants: string[];
  epoch: number;
  pendingTransaction: unknown;
  streamDiagnosticsAvailable: boolean;
  factoryResetAvailable: boolean;
}

export function unwrapRpc<T>(envelope: RpcEnvelope<T>): T;
export function formatUptime(milliseconds: unknown): string;
export function normalizeSnapshot(rawValue: unknown): SnapshotView;
export function normalizeConfig(rawValue: unknown): {
  revision: string;
  pendingTransaction: unknown;
  draft: ConfigDraft;
};
export function buildConfigRequest(revision: string, draft: ConfigDraft): ConfigRequest;
export function transactionId(pending: unknown): string | null;
