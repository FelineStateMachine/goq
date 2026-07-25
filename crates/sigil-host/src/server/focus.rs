use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use sigil_protocol::{
    ControllerSlot, FocusProposalV2, FocusStateV2, FocusTransitionReasonV2, ViewerPresenceId,
};

const FOCUS_PROPOSAL_TTL: Duration = Duration::from_secs(15);
const FOCUS_ACTIVATION_TTL: Duration = Duration::from_secs(10);
const FOCUS_COMMAND_RATE_WINDOW: Duration = Duration::from_secs(2);
const MAX_FOCUS_COMMANDS_PER_WINDOW: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FocusCandidate {
    pub(super) presence_id: ViewerPresenceId,
    pub(super) session_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FocusNeutralization {
    pub(crate) transition_id: u64,
    pub(crate) former_session_id: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct FocusMutation {
    pub(super) changed: bool,
    pub(super) neutralization: Option<FocusNeutralization>,
}

#[derive(Debug)]
struct FocusSuccessor {
    candidate: FocusCandidate,
}

#[derive(Debug)]
struct ActivationDeadline {
    presence_id: ViewerPresenceId,
    session_id: u64,
    focus_generation: u64,
    deadline: Instant,
}

#[derive(Debug)]
pub(super) struct FocusArbiter {
    state: FocusStateV2,
    proposal: Option<FocusProposalV2>,
    proposal_deadline: Option<Instant>,
    transition_reason: FocusTransitionReasonV2,
    configured_owner: Option<ViewerPresenceId>,
    successor: Option<FocusSuccessor>,
    activation: Option<ActivationDeadline>,
    next_focus_generation: u64,
    next_transition_id: u64,
    next_proposal_id: u64,
    command_times: HashMap<(ViewerPresenceId, u64), VecDeque<Instant>>,
}

impl FocusArbiter {
    pub(super) fn new(configured_owner: Option<ViewerPresenceId>) -> Self {
        Self {
            state: FocusStateV2::Vacant {
                slot: ControllerSlot::ZERO,
            },
            proposal: None,
            proposal_deadline: None,
            transition_reason: FocusTransitionReasonV2::Initial,
            configured_owner,
            successor: None,
            activation: None,
            next_focus_generation: 0,
            next_transition_id: 0,
            next_proposal_id: 0,
            command_times: HashMap::new(),
        }
    }

    pub(super) fn state(&self) -> &FocusStateV2 {
        &self.state
    }

    pub(super) fn proposal(&self) -> Option<&FocusProposalV2> {
        self.proposal.as_ref()
    }

    pub(super) fn transition_reason(&self) -> FocusTransitionReasonV2 {
        self.transition_reason
    }

    pub(super) fn is_configured_owner(&self, presence_id: &ViewerPresenceId) -> bool {
        self.configured_owner.as_ref() == Some(presence_id)
    }

    pub(super) fn check_rate_limit(
        &mut self,
        requester: &FocusCandidate,
        now: Instant,
    ) -> Result<()> {
        let key = (requester.presence_id.clone(), requester.session_id);
        let entries = self.command_times.entry(key).or_default();
        while entries
            .front()
            .is_some_and(|time| now.duration_since(*time) >= FOCUS_COMMAND_RATE_WINDOW)
        {
            entries.pop_front();
        }
        ensure!(
            entries.len() < MAX_FOCUS_COMMANDS_PER_WINDOW,
            "focus command rate limit exceeded"
        );
        entries.push_back(now);
        Ok(())
    }

    pub(super) fn expire_proposal(&mut self, now: Instant) -> bool {
        if self.proposal_deadline.is_none_or(|deadline| deadline > now) {
            return false;
        }
        self.clear_proposal();
        self.transition_reason = FocusTransitionReasonV2::ProposalExpired;
        true
    }

    pub(super) fn request(
        &mut self,
        requester: FocusCandidate,
        now: Instant,
    ) -> Result<FocusMutation> {
        self.expire_proposal(now);
        match &self.state {
            FocusStateV2::Vacant { slot } => {
                let slot = *slot;
                self.grant(slot, requester, now)?;
                self.transition_reason = FocusTransitionReasonV2::Requested;
                Ok(FocusMutation {
                    changed: true,
                    neutralization: None,
                })
            }
            FocusStateV2::Held {
                slot,
                holder,
                session_id,
                ..
            } => {
                ensure!(
                    holder != &requester.presence_id || *session_id != requester.session_id,
                    "current holder cannot request focus from itself"
                );
                ensure!(
                    self.proposal.is_none(),
                    "controller slot 0 already has a pending handoff proposal"
                );
                self.next_proposal_id = next_nonzero(self.next_proposal_id, "focus proposal id")?;
                let expires_at_unix_ms = unix_millis_after(FOCUS_PROPOSAL_TTL)?;
                self.proposal = Some(FocusProposalV2 {
                    proposal_id: self.next_proposal_id,
                    slot: *slot,
                    requester: requester.presence_id,
                    requester_session_id: requester.session_id,
                    holder: holder.clone(),
                    holder_session_id: *session_id,
                    expires_at_unix_ms,
                });
                self.proposal_deadline = Some(now + FOCUS_PROPOSAL_TTL);
                self.transition_reason = FocusTransitionReasonV2::HandoffRequested;
                Ok(FocusMutation {
                    changed: true,
                    neutralization: None,
                })
            }
            FocusStateV2::Neutralizing { .. } => {
                bail!("controller slot 0 is neutralizing")
            }
        }
    }

    pub(super) fn approve(
        &mut self,
        holder: &FocusCandidate,
        expected_focus_generation: u64,
        expected_proposal_id: u64,
    ) -> Result<FocusMutation> {
        self.ensure_holder(holder, expected_focus_generation)?;
        let proposal = self.ensure_proposal(holder, expected_proposal_id)?.clone();
        self.begin_neutralization(
            FocusTransitionReasonV2::HandoffApproved,
            Some(FocusCandidate {
                presence_id: proposal.requester,
                session_id: proposal.requester_session_id,
            }),
        )
    }

    pub(super) fn deny(
        &mut self,
        holder: &FocusCandidate,
        expected_focus_generation: u64,
        expected_proposal_id: u64,
    ) -> Result<FocusMutation> {
        self.ensure_holder(holder, expected_focus_generation)?;
        self.ensure_proposal(holder, expected_proposal_id)?;
        self.clear_proposal();
        self.transition_reason = FocusTransitionReasonV2::HandoffDenied;
        Ok(FocusMutation {
            changed: true,
            neutralization: None,
        })
    }

    pub(super) fn release(
        &mut self,
        holder: &FocusCandidate,
        expected_focus_generation: u64,
    ) -> Result<FocusMutation> {
        self.ensure_holder(holder, expected_focus_generation)?;
        let successor = self.proposal.as_ref().map(|proposal| FocusCandidate {
            presence_id: proposal.requester.clone(),
            session_id: proposal.requester_session_id,
        });
        self.begin_neutralization(FocusTransitionReasonV2::Released, successor)
    }

    pub(super) fn preempt(
        &mut self,
        owner: FocusCandidate,
        expected_focus_generation: u64,
    ) -> Result<FocusMutation> {
        ensure!(
            self.is_configured_owner(&owner.presence_id),
            "viewer is not the configured focus owner"
        );
        let FocusStateV2::Held {
            holder,
            session_id,
            focus_generation,
            ..
        } = &self.state
        else {
            bail!("controller slot 0 is not held")
        };
        ensure!(
            *focus_generation == expected_focus_generation,
            "preemption expected a stale focus generation"
        );
        ensure!(
            holder != &owner.presence_id || *session_id != owner.session_id,
            "configured owner already holds focus"
        );
        self.begin_neutralization(FocusTransitionReasonV2::Preempted, Some(owner))
    }

    pub(super) fn begin_invalidation(
        &mut self,
        candidate: &FocusCandidate,
        reason: FocusTransitionReasonV2,
    ) -> Result<FocusMutation> {
        let proposal_involved = self.proposal.as_ref().is_some_and(|proposal| {
            proposal.requester == candidate.presence_id
                && proposal.requester_session_id == candidate.session_id
                || proposal.holder == candidate.presence_id
                    && proposal.holder_session_id == candidate.session_id
        });
        if proposal_involved {
            self.clear_proposal();
        }
        let held = matches!(
            &self.state,
            FocusStateV2::Held { holder, session_id, .. }
                if holder == &candidate.presence_id && *session_id == candidate.session_id
        );
        if held {
            return self.begin_neutralization(reason, None);
        }
        if proposal_involved {
            self.transition_reason = reason;
        }
        Ok(FocusMutation {
            changed: proposal_involved,
            neutralization: None,
        })
    }

    pub(super) fn mark_activated(&mut self, candidate: &FocusCandidate, focus_generation: u64) {
        if self.activation.as_ref().is_some_and(|activation| {
            activation.presence_id == candidate.presence_id
                && activation.session_id == candidate.session_id
                && activation.focus_generation == focus_generation
        }) {
            self.activation = None;
        }
    }

    pub(super) fn begin_activation_expiry(&mut self, now: Instant) -> Result<FocusMutation> {
        let Some(activation) = self.activation.as_ref() else {
            return Ok(FocusMutation::default());
        };
        if activation.deadline > now {
            return Ok(FocusMutation::default());
        }
        let candidate = FocusCandidate {
            presence_id: activation.presence_id.clone(),
            session_id: activation.session_id,
        };
        let focus_generation = activation.focus_generation;
        self.ensure_holder(&candidate, focus_generation)?;
        self.begin_neutralization(FocusTransitionReasonV2::ActivationExpired, None)
    }

    pub(super) fn transition_successor(&self, transition_id: u64) -> Option<&FocusCandidate> {
        match &self.state {
            FocusStateV2::Neutralizing {
                transition_id: current,
                ..
            } if *current == transition_id => self
                .successor
                .as_ref()
                .map(|successor| &successor.candidate),
            _ => None,
        }
    }

    pub(super) fn complete_transition(
        &mut self,
        transition_id: u64,
        successor_is_valid: bool,
        now: Instant,
    ) -> Result<bool> {
        let FocusStateV2::Neutralizing {
            slot,
            transition_id: current,
            ..
        } = self.state
        else {
            return Ok(false);
        };
        if current != transition_id {
            return Ok(false);
        }
        let successor = self.successor.take().filter(|_| successor_is_valid);
        self.clear_proposal();
        if let Some(successor) = successor {
            self.grant(slot, successor.candidate, now)?;
            if self.transition_reason == FocusTransitionReasonV2::Released {
                self.transition_reason = FocusTransitionReasonV2::HandoffApproved;
            }
        } else {
            self.state = FocusStateV2::Vacant { slot };
            self.activation = None;
        }
        Ok(true)
    }

    fn ensure_holder(
        &self,
        candidate: &FocusCandidate,
        expected_focus_generation: u64,
    ) -> Result<()> {
        ensure!(
            matches!(
                &self.state,
                FocusStateV2::Held {
                    holder,
                    session_id,
                    focus_generation,
                    ..
                } if holder == &candidate.presence_id
                    && *session_id == candidate.session_id
                    && *focus_generation == expected_focus_generation
            ),
            "focus command does not match the current holder generation"
        );
        Ok(())
    }

    fn ensure_proposal(
        &self,
        holder: &FocusCandidate,
        expected_proposal_id: u64,
    ) -> Result<&FocusProposalV2> {
        self.proposal
            .as_ref()
            .filter(|proposal| {
                proposal.proposal_id == expected_proposal_id
                    && proposal.holder == holder.presence_id
                    && proposal.holder_session_id == holder.session_id
            })
            .context("handoff decision does not match the pending proposal")
    }

    fn begin_neutralization(
        &mut self,
        reason: FocusTransitionReasonV2,
        successor: Option<FocusCandidate>,
    ) -> Result<FocusMutation> {
        let FocusStateV2::Held {
            slot,
            holder,
            session_id,
            focus_generation,
        } = self.state.clone()
        else {
            bail!("controller slot 0 is not held")
        };
        self.next_transition_id = next_nonzero(self.next_transition_id, "focus transition id")?;
        self.state = FocusStateV2::Neutralizing {
            slot,
            former_holder: holder,
            former_session_id: session_id,
            former_focus_generation: focus_generation,
            transition_id: self.next_transition_id,
        };
        self.transition_reason = reason;
        self.successor = successor.map(|candidate| FocusSuccessor { candidate });
        self.activation = None;
        self.clear_proposal();
        Ok(FocusMutation {
            changed: true,
            neutralization: Some(FocusNeutralization {
                transition_id: self.next_transition_id,
                former_session_id: session_id,
            }),
        })
    }

    fn grant(
        &mut self,
        slot: ControllerSlot,
        candidate: FocusCandidate,
        now: Instant,
    ) -> Result<()> {
        self.next_focus_generation = next_nonzero(self.next_focus_generation, "focus generation")?;
        self.state = FocusStateV2::Held {
            slot,
            holder: candidate.presence_id.clone(),
            session_id: candidate.session_id,
            focus_generation: self.next_focus_generation,
        };
        self.activation = Some(ActivationDeadline {
            presence_id: candidate.presence_id,
            session_id: candidate.session_id,
            focus_generation: self.next_focus_generation,
            deadline: now + FOCUS_ACTIVATION_TTL,
        });
        Ok(())
    }

    fn clear_proposal(&mut self) {
        self.proposal = None;
        self.proposal_deadline = None;
    }
}

fn next_nonzero(current: u64, name: &'static str) -> Result<u64> {
    current
        .checked_add(1)
        .with_context(|| format!("{name} exhausted"))
}

fn unix_millis_after(duration: Duration) -> Result<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    u64::try_from((now + duration).as_millis()).context("focus proposal expiry overflowed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(handle: &str, session_id: u64) -> FocusCandidate {
        FocusCandidate {
            presence_id: ViewerPresenceId::new(handle).unwrap(),
            session_id,
        }
    }

    #[test]
    fn handoff_publishes_neutralizing_before_successor_generation() {
        let now = Instant::now();
        let first = candidate("viewer-0000000000000001", 1);
        let second = candidate("viewer-0000000000000002", 2);
        let mut focus = FocusArbiter::new(None);
        focus.request(first.clone(), now).unwrap();
        let generation = match focus.state() {
            FocusStateV2::Held {
                focus_generation, ..
            } => *focus_generation,
            _ => panic!("focus was not granted"),
        };
        focus.request(second.clone(), now).unwrap();
        let proposal_id = focus.proposal().unwrap().proposal_id;
        let mutation = focus.approve(&first, generation, proposal_id).unwrap();
        let transition = mutation.neutralization.unwrap();
        assert!(matches!(focus.state(), FocusStateV2::Neutralizing { .. }));
        assert_eq!(
            focus.transition_successor(transition.transition_id),
            Some(&second)
        );
        focus
            .complete_transition(transition.transition_id, true, now)
            .unwrap();
        assert!(matches!(
            focus.state(),
            FocusStateV2::Held { holder, session_id: 2, focus_generation, .. }
                if holder == &second.presence_id && *focus_generation > generation
        ));
    }

    #[test]
    fn only_configured_owner_can_preempt_and_commands_are_rate_limited() {
        let now = Instant::now();
        let owner = candidate("viewer-0000000000000001", 1);
        let holder = candidate("viewer-0000000000000002", 2);
        let mut focus = FocusArbiter::new(Some(owner.presence_id.clone()));
        focus.request(holder.clone(), now).unwrap();
        let generation = match focus.state() {
            FocusStateV2::Held {
                focus_generation, ..
            } => *focus_generation,
            _ => unreachable!(),
        };
        assert!(
            focus
                .preempt(candidate("viewer-0000000000000003", 3), generation)
                .is_err()
        );
        assert!(focus.preempt(owner.clone(), generation).is_ok());

        let mut limited = FocusArbiter::new(None);
        for _ in 0..MAX_FOCUS_COMMANDS_PER_WINDOW {
            limited.check_rate_limit(&owner, now).unwrap();
        }
        assert!(limited.check_rate_limit(&owner, now).is_err());
    }

    #[test]
    fn proposals_expire_without_creating_a_queue() {
        let now = Instant::now();
        let first = candidate("viewer-0000000000000001", 1);
        let second = candidate("viewer-0000000000000002", 2);
        let third = candidate("viewer-0000000000000003", 3);
        let mut focus = FocusArbiter::new(None);
        focus.request(first, now).unwrap();
        focus.request(second, now).unwrap();
        assert!(focus.request(third.clone(), now).is_err());
        assert!(focus.expire_proposal(now + FOCUS_PROPOSAL_TTL));
        assert!(focus.request(third, now + FOCUS_PROPOSAL_TTL).is_ok());
    }
}
