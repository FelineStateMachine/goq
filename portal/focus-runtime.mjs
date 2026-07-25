function currentProposalForHolder(session) {
  const proposal = session.snapshot?.focus_proposal;
  const focus = session.snapshot?.focus;
  if (!proposal || focus?.state !== 'held' || !session.focused) return null;
  if (proposal.holder !== session.snapshot.self_presence_id
    || proposal.holder_session_id !== session.snapshot.viewers.find((viewer) => viewer.you)?.session_id) {
    return null;
  }
  return proposal;
}

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

  async function queue(kind, command, args, controllerInitiated = false) {
    if (pending !== null) return false;
    const requestId = nextRequestId++;
    pending = { requestId, kind, command, controllerInitiated };
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
    return queue('request', 'iroh_client_request_focus', {
      generation: session.nativeGeneration,
      expectedRevision: session.revision,
    }, controllerInitiated);
  }

  async function approve() {
    const session = getSession();
    const proposal = currentProposalForHolder(session);
    if (!proposal) return false;
    return queue('approve', 'iroh_client_approve_focus', {
      generation: session.nativeGeneration,
      expectedRevision: session.revision,
      expectedFocusGeneration: session.focusGeneration,
      expectedProposalId: proposal.proposal_id,
    });
  }

  async function deny() {
    const session = getSession();
    const proposal = currentProposalForHolder(session);
    if (!proposal) return false;
    return queue('deny', 'iroh_client_deny_focus', {
      generation: session.nativeGeneration,
      expectedRevision: session.revision,
      expectedFocusGeneration: session.focusGeneration,
      expectedProposalId: proposal.proposal_id,
    });
  }

  async function release() {
    const session = getSession();
    if (session.mode !== 'control-v2' || !session.focused) return false;
    return queue('release', 'iroh_client_release_focus', {
      generation: session.nativeGeneration,
      expectedRevision: session.revision,
      expectedFocusGeneration: session.focusGeneration,
    });
  }

  async function preempt({ controllerInitiated = false } = {}) {
    const session = getSession();
    const focus = session.snapshot?.focus;
    if (session.mode !== 'control-v2'
      || !session.eligible
      || session.focused
      || session.snapshot?.self_is_focus_owner !== true
      || focus?.state !== 'held') return false;
    return queue('preempt', 'iroh_client_preempt_focus', {
      generation: session.nativeGeneration,
      expectedRevision: session.revision,
      expectedFocusGeneration: focus.focus_generation,
    }, controllerInitiated);
  }

  async function observeSession() {
    const session = getSession();
    const request = pending;
    if (!request) return false;
    if ((request.kind === 'request' || request.kind === 'preempt') && session.focused) {
      pending = null;
      onChange(snapshot());
      await startFocusedInput();
      await activateLocalControl({ controllerInitiated: request.controllerInitiated });
      return true;
    }
    if (request.kind === 'release' && !session.focused) {
      pending = null;
      onChange(snapshot());
      return true;
    }
    if (request.kind === 'request') {
      const proposal = session.snapshot?.focus_proposal;
      const selfId = session.snapshot?.self_presence_id;
      const stillPending = proposal?.requester === selfId;
      const terminalReason = ['handoff_denied', 'proposal_expired', 'disconnected', 'replaced', 'revoked']
        .includes(session.snapshot?.transition_reason);
      if (!stillPending && terminalReason) {
        pending = null;
        onChange(snapshot());
      }
    }
    return false;
  }

  function observeResult(result) {
    if (!pending || result?.request_id !== pending.requestId) return false;
    if (result.accepted === false || ['approve', 'deny'].includes(pending.kind)) {
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
    return {
      pending: pending !== null,
      kind: pending?.kind ?? null,
      requestId: pending?.requestId ?? null,
    };
  }

  return Object.freeze({
    approve,
    deny,
    preempt,
    release,
    request,
    observeSession,
    observeResult,
    reset,
    snapshot,
  });
}
