export function createFocusRuntime({
  invokeCommand,
  getSession,
  startFocusedInput,
  activateLocalControl,
  onChange = () => {},
} = {}) {
  if (typeof invokeCommand !== 'function') throw new TypeError('invokeCommand must be a function');
  if (typeof getSession !== 'function') throw new TypeError('getSession must be a function');
  if (typeof startFocusedInput !== 'function') throw new TypeError('startFocusedInput must be a function');
  if (typeof activateLocalControl !== 'function') throw new TypeError('activateLocalControl must be a function');
  let nextRequestId = 1;
  let pending = null;

  async function queue(command, args, controllerInitiated = false) {
    if (pending !== null) return false;
    const requestId = nextRequestId++;
    pending = { requestId, command, controllerInitiated };
    onChange(snapshot());
    try {
      const queued = await invokeCommand(command, { requestId, ...args });
      if (!queued && pending?.requestId === requestId) pending = null;
      onChange(snapshot());
      return queued === true;
    } catch (error) {
      if (pending?.requestId === requestId) pending = null;
      onChange(snapshot());
      throw error;
    }
  }

  async function request({ controllerInitiated = false } = {}) {
    const session = getSession();
    if (session.mode !== 'control-v2' || !session.eligible || session.focused) return false;
    return queue('iroh_client_request_focus', {
      generation: session.nativeGeneration,
      expectedRevision: session.revision,
    }, controllerInitiated);
  }

  async function release() {
    const session = getSession();
    if (session.mode !== 'control-v2' || !session.focused) return false;
    return queue('iroh_client_release_focus', {
      generation: session.nativeGeneration,
      expectedRevision: session.revision,
      expectedFocusGeneration: session.focusGeneration,
    });
  }

  async function observeSession() {
    const session = getSession();
    const request = pending;
    if (!request || request.command !== 'iroh_client_request_focus' || !session.focused) return false;
    pending = null;
    onChange(snapshot());
    await startFocusedInput();
    await activateLocalControl({ controllerInitiated: request.controllerInitiated });
    return true;
  }

  function observeResult(result) {
    if (!pending || result?.request_id !== pending.requestId) return false;
    if (result.accepted === false) {
      pending = null;
      onChange(snapshot());
    }
    return true;
  }

  function reset() {
    pending = null;
    onChange(snapshot());
  }

  function snapshot() {
    return { pending: pending !== null, requestId: pending?.requestId ?? null };
  }

  return Object.freeze({ request, release, observeSession, observeResult, reset, snapshot });
}
