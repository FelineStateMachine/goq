use std::collections::BTreeMap;
use std::time::Duration;

#[cfg(test)]
use sigil_protocol::MediaFrame;
use sigil_protocol::{
    FrameFlags, KeyframeRequestReasonV3, MAX_MEDIA_GROUP_BYTES_V3, MediaObjectV3, ProtocolError,
    read_media_object_v3,
};

const CLIENT_MEDIA_OBJECT_CAPACITY: usize = 4;
const CLIENT_MEDIA_OBJECT_READ_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) enum MediaObjectReadOutcomeV3 {
    Object {
        accept_index: u64,
        object: MediaObjectV3,
    },
    Dropped {
        accept_index: u64,
        reason: KeyframeRequestReasonV3,
    },
    Malformed(String),
}

impl MediaObjectReadOutcomeV3 {
    fn accept_index(&self) -> Option<u64> {
        match self {
            Self::Object { accept_index, .. } | Self::Dropped { accept_index, .. } => {
                Some(*accept_index)
            }
            Self::Malformed(_) => None,
        }
    }

    fn is_fast_forward_barrier(&self) -> bool {
        let Self::Object { object, .. } = self else {
            return false;
        };
        object.header.object_id == 0
            && object.header.flags.contains(FrameFlags::KEYFRAME)
            && object.header.flags.contains(FrameFlags::CODEC_CONFIG)
            && object.header.flags.contains(FrameFlags::DISCONTINUITY)
    }
}

#[derive(Debug)]
struct MediaObjectReorderV3 {
    next_accept_index: u64,
    completed: BTreeMap<u64, MediaObjectReadOutcomeV3>,
}

impl MediaObjectReorderV3 {
    fn new(first_accept_index: u64) -> Self {
        Self {
            next_accept_index: first_accept_index,
            completed: BTreeMap::new(),
        }
    }

    fn pending_len(&self) -> usize {
        self.completed.len()
    }

    fn push(
        &mut self,
        outcome: MediaObjectReadOutcomeV3,
    ) -> Result<Option<MediaObjectReadOutcomeV3>, String> {
        let Some(accept_index) = outcome.accept_index() else {
            return Ok(Some(outcome));
        };
        if accept_index < self.next_accept_index {
            // A discontinuity barrier may advance beyond older in-flight
            // reads. Their eventual timeout/reset outcomes belong to the
            // superseded GOP and must not poison the recovered sequence.
            return Ok(None);
        }
        if self.completed.insert(accept_index, outcome).is_some() {
            return Err(format!(
                "Duplicate media v3 accept index {accept_index} completed"
            ));
        }
        if self
            .completed
            .get(&accept_index)
            .is_some_and(MediaObjectReadOutcomeV3::is_fast_forward_barrier)
        {
            self.completed.retain(|index, _| *index >= accept_index);
            self.next_accept_index = accept_index;
        }
        self.take_next()
    }

    fn take_next(&mut self) -> Result<Option<MediaObjectReadOutcomeV3>, String> {
        let Some(outcome) = self.completed.remove(&self.next_accept_index) else {
            return Ok(None);
        };
        self.next_accept_index = self
            .next_accept_index
            .checked_add(1)
            .ok_or_else(|| "Media v3 accept index overflowed".to_string())?;
        Ok(Some(outcome))
    }
}

pub(crate) struct MediaObjectReceiverV3 {
    connection: iroh::endpoint::Connection,
    reads: tokio::task::JoinSet<MediaObjectReadOutcomeV3>,
    reorder: MediaObjectReorderV3,
    next_accept_index: u64,
    connection_closed: bool,
}

impl MediaObjectReceiverV3 {
    pub(crate) fn new(connection: iroh::endpoint::Connection) -> Self {
        Self {
            connection,
            reads: tokio::task::JoinSet::new(),
            reorder: MediaObjectReorderV3::new(1),
            next_accept_index: 0,
            connection_closed: false,
        }
    }

    pub(crate) async fn next(&mut self) -> Result<Option<MediaObjectReadOutcomeV3>, String> {
        loop {
            if let Some(completed) = self.reorder.take_next()? {
                return Ok(Some(completed));
            }
            if self.connection_closed && self.reads.is_empty() {
                if self.reorder.pending_len() != 0 {
                    return Err("Media v3 connection closed with incomplete object order".into());
                }
                return Ok(None);
            }

            tokio::select! {
                biased;
                completed = self.reads.join_next(), if !self.reads.is_empty() => {
                    let completed = completed
                        .ok_or_else(|| "Media v3 object reader ended unexpectedly".to_string())?
                        .map_err(|error| format!("Media v3 object reader task failed: {error}"))?;
                    if let Some(completed) = self.reorder.push(completed)? {
                        return Ok(Some(completed));
                    }
                }
                accepted = self.connection.accept_uni(), if !self.connection_closed
                    && self.reads.len() + self.reorder.pending_len()
                        < CLIENT_MEDIA_OBJECT_CAPACITY => {
                    let mut stream = match accepted {
                        Ok(stream) => stream,
                        Err(_) => {
                            self.connection_closed = true;
                            continue;
                        }
                    };
                    self.next_accept_index = self.next_accept_index.checked_add(1)
                        .ok_or_else(|| "Media v3 accept index overflowed".to_string())?;
                    let accept_index = self.next_accept_index;
                    self.reads.spawn(async move {
                        match tokio::time::timeout(
                            CLIENT_MEDIA_OBJECT_READ_TIMEOUT,
                            read_media_object_v3(&mut stream),
                        )
                        .await
                        {
                            Err(_) => MediaObjectReadOutcomeV3::Dropped {
                                accept_index,
                                reason: KeyframeRequestReasonV3::DeliveryTimeout,
                            },
                            Ok(Err(ProtocolError::Io(_))) => MediaObjectReadOutcomeV3::Dropped {
                                accept_index,
                                reason: KeyframeRequestReasonV3::TransportGap,
                            },
                            Ok(Err(error)) => MediaObjectReadOutcomeV3::Malformed(format!(
                                "Invalid media v3 object: {error}"
                            )),
                            Ok(Ok(object)) => MediaObjectReadOutcomeV3::Object {
                                accept_index,
                                object,
                            },
                        }
                    });
                }
            }
        }
    }
}

impl Drop for MediaObjectReceiverV3 {
    fn drop(&mut self) {
        self.reads.abort_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaObjectSequenceDecisionV3 {
    Deliver { discontinuity: bool },
    DropLate,
    DropUntilKeyframe,
}

#[derive(Debug, Default)]
pub(crate) struct MediaObjectSequenceV3 {
    group_id: Option<u64>,
    last_object_id: Option<u32>,
    last_sequence: Option<u64>,
    group_payload_bytes: usize,
    waiting_for_keyframe: bool,
}

impl MediaObjectSequenceV3 {
    pub(crate) fn new() -> Self {
        Self {
            waiting_for_keyframe: true,
            ..Self::default()
        }
    }

    pub(crate) fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    pub(crate) fn waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    pub(crate) fn note_dropped_object(&mut self) -> bool {
        let entered = !self.waiting_for_keyframe;
        self.waiting_for_keyframe = true;
        entered
    }

    pub(crate) fn classify(&mut self, object: &MediaObjectV3) -> MediaObjectSequenceDecisionV3 {
        let header = &object.header;
        if self
            .group_id
            .is_some_and(|group_id| header.group_id < group_id)
            || self
                .last_sequence
                .is_some_and(|sequence| header.sequence <= sequence)
        {
            return MediaObjectSequenceDecisionV3::DropLate;
        }

        let new_group = self.group_id != Some(header.group_id);
        let recovery_keyframe = header.object_id == 0
            && header.flags.contains(FrameFlags::KEYFRAME)
            && header.flags.contains(FrameFlags::CODEC_CONFIG);
        if new_group && !recovery_keyframe {
            self.waiting_for_keyframe = true;
            return MediaObjectSequenceDecisionV3::DropUntilKeyframe;
        }
        if !new_group && self.waiting_for_keyframe {
            return MediaObjectSequenceDecisionV3::DropUntilKeyframe;
        }

        let sequence_contiguous = self
            .last_sequence
            .is_none_or(|sequence| sequence.checked_add(1) == Some(header.sequence));
        let object_contiguous = new_group
            || self
                .last_object_id
                .is_some_and(|object_id| object_id.checked_add(1) == Some(header.object_id));
        let next_group_bytes = if new_group {
            object.payload.len()
        } else {
            self.group_payload_bytes
                .saturating_add(object.payload.len())
        };
        if (!sequence_contiguous && !new_group)
            || !object_contiguous
            || next_group_bytes > MAX_MEDIA_GROUP_BYTES_V3
        {
            self.waiting_for_keyframe = true;
            return MediaObjectSequenceDecisionV3::DropUntilKeyframe;
        }

        let discontinuity = header.flags.contains(FrameFlags::DISCONTINUITY)
            || self.waiting_for_keyframe
            || !sequence_contiguous;
        self.group_id = Some(header.group_id);
        self.last_object_id = Some(header.object_id);
        self.last_sequence = Some(header.sequence);
        self.group_payload_bytes = next_group_bytes;
        self.waiting_for_keyframe = false;
        MediaObjectSequenceDecisionV3::Deliver { discontinuity }
    }
}

#[cfg(test)]
pub(crate) fn media_object_frame(sequence: u64, flags: FrameFlags) -> MediaFrame {
    let payload = vec![sequence as u8];
    let header = sigil_protocol::MediaFrameHeader::h264(
        1280,
        800,
        payload.len(),
        sequence,
        sequence * 1_000,
        sequence as i64 * 1_000,
        flags,
    )
    .unwrap();
    MediaFrame::new(header, payload).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_object_v3(
        group_id: u64,
        object_id: u32,
        sequence: u64,
        flags: FrameFlags,
    ) -> MediaObjectV3 {
        let payload = vec![sequence as u8];
        let header = sigil_protocol::MediaObjectHeaderV3::h264(
            1280,
            800,
            payload.len(),
            if object_id == 0 { 0 } else { 128 },
            flags,
            object_id,
            group_id,
            sequence,
            sequence * 1_000,
            sequence as i64 * 1_000,
            100,
        )
        .unwrap();
        MediaObjectV3::new(header, payload).unwrap()
    }

    fn media_object_outcome_v3(
        accept_index: u64,
        group_id: u64,
        object_id: u32,
        sequence: u64,
        flags: FrameFlags,
    ) -> MediaObjectReadOutcomeV3 {
        MediaObjectReadOutcomeV3::Object {
            accept_index,
            object: media_object_v3(group_id, object_id, sequence, flags),
        }
    }

    #[test]
    fn media_v3_groups_use_wire_object_identity_and_recover_on_new_group_zero() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequenceV3::new();

        assert_eq!(
            sequence.classify(&media_object_v3(10, 0, 10, keyframe)),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 1, 11, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: false
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 3, 13, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 2, 12, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(&media_object_v3(
                20,
                0,
                20,
                keyframe.union(FrameFlags::DISCONTINUITY),
            )),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(
            sequence.classify(&media_object_v3(10, 2, 12, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::DropLate
        );
    }

    #[test]
    fn late_dropped_completion_after_v3_barrier_cannot_poison_recovered_group() {
        let barrier = FrameFlags::KEYFRAME
            .union(FrameFlags::CODEC_CONFIG)
            .union(FrameFlags::DISCONTINUITY);
        let mut reorder = MediaObjectReorderV3::new(1);
        let mut sequence = MediaObjectSequenceV3::new();

        let recovered = reorder
            .push(media_object_outcome_v3(5, 20, 0, 20, barrier))
            .unwrap()
            .unwrap();
        let MediaObjectReadOutcomeV3::Object { object, .. } = recovered else {
            panic!("recovery barrier must remain an object");
        };
        assert_eq!(
            sequence.classify(&object),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: true
            }
        );

        assert!(
            reorder
                .push(MediaObjectReadOutcomeV3::Dropped {
                    accept_index: 1,
                    reason: KeyframeRequestReasonV3::DeliveryTimeout,
                })
                .unwrap()
                .is_none()
        );

        let delta = reorder
            .push(media_object_outcome_v3(6, 20, 1, 21, FrameFlags::NONE))
            .unwrap()
            .unwrap();
        let MediaObjectReadOutcomeV3::Object { object, .. } = delta else {
            panic!("new group delta must remain an object");
        };
        assert_eq!(
            sequence.classify(&object),
            MediaObjectSequenceDecisionV3::Deliver {
                discontinuity: false
            }
        );
    }

    #[test]
    fn media_v3_receiver_rejects_group_payload_growth_beyond_the_shared_cap() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequenceV3::new();
        assert!(matches!(
            sequence.classify(&media_object_v3(1, 0, 1, keyframe)),
            MediaObjectSequenceDecisionV3::Deliver { .. }
        ));
        sequence.group_payload_bytes = MAX_MEDIA_GROUP_BYTES_V3;
        assert_eq!(
            sequence.classify(&media_object_v3(1, 1, 2, FrameFlags::NONE)),
            MediaObjectSequenceDecisionV3::DropUntilKeyframe
        );
    }
}
