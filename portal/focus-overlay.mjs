function selfViewer(snapshot) {
  return snapshot?.viewers?.find((viewer) => viewer.you === true) ?? null;
}

export function handoffProposalForSelf(session) {
  const snapshot = session?.snapshot;
  const proposal = snapshot?.focus_proposal;
  const self = selfViewer(snapshot);
  if (!proposal || !self || !session.focused) return null;
  if (proposal.holder !== self.presence_id || proposal.holder_session_id !== self.session_id) {
    return null;
  }
  return Object.freeze({
    type: 'handoff',
    proposalId: proposal.proposal_id,
    requester: proposal.requester,
    revision: snapshot.revision,
  });
}

export function preemptionCandidateForSelf(session) {
  const snapshot = session?.snapshot;
  const focus = snapshot?.focus;
  if (snapshot?.self_is_focus_owner !== true
    || !session.eligible
    || session.focused
    || focus?.state !== 'held') return null;
  return Object.freeze({
    type: 'preempt',
    holder: focus.holder,
    holderSessionId: focus.session_id,
    focusGeneration: focus.focus_generation,
    revision: snapshot.revision,
  });
}

export function focusOverlayStillCurrent(overlay, session) {
  if (!overlay) return false;
  if (overlay.type === 'handoff') {
    const current = handoffProposalForSelf(session);
    return current?.proposalId === overlay.proposalId && current.revision >= overlay.revision;
  }
  if (overlay.type === 'preempt') {
    const current = preemptionCandidateForSelf(session);
    return current?.holder === overlay.holder
      && current.holderSessionId === overlay.holderSessionId
      && current.focusGeneration === overlay.focusGeneration
      && current.revision >= overlay.revision;
  }
  return false;
}
