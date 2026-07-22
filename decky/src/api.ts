import { callable } from "@decky/api";
import type { ConfigRequest, RpcEnvelope } from "./model.mjs";

export const getSnapshot = callable<[], RpcEnvelope<unknown>>("get_snapshot");
export const getConfig = callable<[], RpcEnvelope<unknown>>("get_config");
export const validateConfig = callable<[request: ConfigRequest], RpcEnvelope<unknown>>("validate_config");
export const applyConfig = callable<[request: ConfigRequest], RpcEnvelope<unknown>>("apply_config");
export const restartService = callable<[], RpcEnvelope<unknown>>("restart_service");
export const rollbackPending = callable<[transaction: string], RpcEnvelope<unknown>>("rollback_pending");
export const resetEnrollment = callable<
  [expectedHostFingerprint: string],
  RpcEnvelope<unknown>
>("reset_enrollment");
