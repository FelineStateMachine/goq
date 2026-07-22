use std::collections::BTreeMap;
use std::time::Duration;

use sigil_protocol::{FrameFlags, MediaFrame, ProtocolError, read_media_object};

pub(crate) const CLIENT_MEDIA_OBJECT_CAPACITY: usize = 4;
pub(crate) const CLIENT_MEDIA_OBJECT_READ_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) enum MediaObjectReadOutcome {
    Frame {
        object_index: u64,
        frame: MediaFrame,
    },
    Dropped {
        object_index: u64,
    },
    Malformed(String),
}

impl MediaObjectReadOutcome {
    fn object_index(&self) -> Option<u64> {
        match self {
            Self::Frame { object_index, .. } | Self::Dropped { object_index } => {
                Some(*object_index)
            }
            Self::Malformed(_) => None,
        }
    }

    fn is_fast_forward_barrier(&self) -> bool {
        let Self::Frame { frame, .. } = self else {
            return false;
        };
        frame.header.flags.contains(FrameFlags::KEYFRAME)
            && frame.header.flags.contains(FrameFlags::CODEC_CONFIG)
            && frame.header.flags.contains(FrameFlags::DISCONTINUITY)
    }
}

#[derive(Debug)]
struct MediaObjectReorder {
    next_object_index: u64,
    completed: BTreeMap<u64, MediaObjectReadOutcome>,
}

impl MediaObjectReorder {
    fn new(first_object_index: u64) -> Self {
        Self {
            next_object_index: first_object_index,
            completed: BTreeMap::new(),
        }
    }

    fn pending_len(&self) -> usize {
        self.completed.len()
    }

    fn push(
        &mut self,
        outcome: MediaObjectReadOutcome,
    ) -> Result<Option<MediaObjectReadOutcome>, String> {
        let Some(object_index) = outcome.object_index() else {
            // Malformed objects remain terminal as soon as their read completes.
            return Ok(Some(outcome));
        };
        if object_index < self.next_object_index {
            return Ok(Some(outcome));
        }
        if outcome.is_fast_forward_barrier() {
            self.completed
                .retain(|completed_index, _| *completed_index > object_index);
            self.next_object_index = object_index
                .checked_add(1)
                .ok_or_else(|| "Media object reorder index overflowed".to_string())?;
            return Ok(Some(outcome));
        }
        if self.completed.insert(object_index, outcome).is_some() {
            return Err(format!(
                "Media object {object_index} completed more than once"
            ));
        }
        self.take_next()
    }

    fn take_next(&mut self) -> Result<Option<MediaObjectReadOutcome>, String> {
        let Some(outcome) = self.completed.remove(&self.next_object_index) else {
            return Ok(None);
        };
        self.next_object_index = self
            .next_object_index
            .checked_add(1)
            .ok_or_else(|| "Media object reorder index overflowed".to_string())?;
        Ok(Some(outcome))
    }
}

pub(crate) struct MediaObjectReceiver {
    connection: iroh::endpoint::Connection,
    reads: tokio::task::JoinSet<MediaObjectReadOutcome>,
    reorder: MediaObjectReorder,
    next_object_index: u64,
    connection_closed: bool,
}

impl MediaObjectReceiver {
    pub(crate) fn new(connection: iroh::endpoint::Connection) -> Self {
        Self {
            connection,
            reads: tokio::task::JoinSet::new(),
            reorder: MediaObjectReorder::new(1),
            next_object_index: 0,
            connection_closed: false,
        }
    }

    pub(crate) async fn next(&mut self) -> Result<Option<MediaObjectReadOutcome>, String> {
        loop {
            if let Some(completed) = self.reorder.take_next()? {
                return Ok(Some(completed));
            }
            if self.connection_closed && self.reads.is_empty() {
                if self.reorder.pending_len() != 0 {
                    return Err("Media connection closed with an incomplete object order".into());
                }
                return Ok(None);
            }

            tokio::select! {
                biased;
                completed = self.reads.join_next(), if !self.reads.is_empty() => {
                    let completed = completed
                        .ok_or_else(|| "Media object reader ended unexpectedly".to_string())?
                        .map_err(|error| format!("Media object reader task failed: {error}"))?;
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
                    self.next_object_index = self.next_object_index.checked_add(1)
                        .ok_or_else(|| "Media object index overflowed".to_string())?;
                    let object_index = self.next_object_index;
                    self.reads.spawn(async move {
                        match tokio::time::timeout(
                            CLIENT_MEDIA_OBJECT_READ_TIMEOUT,
                            read_media_object(&mut stream),
                        )
                        .await
                        {
                            Err(_) => MediaObjectReadOutcome::Dropped { object_index },
                            Ok(Err(ProtocolError::Io(_))) => {
                                MediaObjectReadOutcome::Dropped { object_index }
                            }
                            Ok(Err(error)) => {
                                MediaObjectReadOutcome::Malformed(format!(
                                    "Invalid media object: {error}"
                                ))
                            }
                            Ok(Ok(frame)) => MediaObjectReadOutcome::Frame {
                                object_index,
                                frame,
                            },
                        }
                    });
                }
            }
        }
    }
}

impl Drop for MediaObjectReceiver {
    fn drop(&mut self) {
        self.reads.abort_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaObjectSequenceDecision {
    Deliver { discontinuity: bool },
    DropLate,
    DropUntilKeyframe,
}

#[derive(Debug, Default)]
pub(crate) struct MediaObjectSequence {
    last_sequence: Option<u64>,
    last_object_index: u64,
    waiting_for_keyframe: bool,
}

impl MediaObjectSequence {
    pub(crate) fn new() -> Self {
        Self {
            waiting_for_keyframe: true,
            ..Self::default()
        }
    }

    pub(crate) fn note_dropped_object(&mut self, object_index: u64) -> bool {
        if object_index <= self.last_object_index {
            return false;
        }
        self.waiting_for_keyframe = true;
        true
    }

    pub(crate) fn classify(
        &mut self,
        object_index: u64,
        frame: &MediaFrame,
    ) -> MediaObjectSequenceDecision {
        if object_index <= self.last_object_index
            || self
                .last_sequence
                .is_some_and(|sequence| frame.header.sequence <= sequence)
        {
            return MediaObjectSequenceDecision::DropLate;
        }

        let keyframe = frame.header.flags.contains(FrameFlags::KEYFRAME)
            && frame.header.flags.contains(FrameFlags::CODEC_CONFIG);
        let sequence_contiguous = self
            .last_sequence
            .is_none_or(|sequence| sequence.checked_add(1) == Some(frame.header.sequence));
        if !keyframe && (self.waiting_for_keyframe || !sequence_contiguous) {
            self.waiting_for_keyframe = true;
            return MediaObjectSequenceDecision::DropUntilKeyframe;
        }

        let discontinuity = frame.header.flags.contains(FrameFlags::DISCONTINUITY)
            || self.waiting_for_keyframe
            || !sequence_contiguous;
        self.last_sequence = Some(frame.header.sequence);
        self.last_object_index = object_index;
        self.waiting_for_keyframe = false;
        MediaObjectSequenceDecision::Deliver { discontinuity }
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

    fn media_object_outcome(
        object_index: u64,
        sequence: u64,
        flags: FrameFlags,
    ) -> MediaObjectReadOutcome {
        MediaObjectReadOutcome::Frame {
            object_index,
            frame: media_object_frame(sequence, flags),
        }
    }

    fn completed_object_index(outcome: &MediaObjectReadOutcome) -> Option<u64> {
        outcome.object_index()
    }

    #[test]
    fn media_object_reorder_restores_accept_order_without_false_resync() {
        let keyframe = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut reorder = MediaObjectReorder::new(1);

        assert!(
            reorder
                .push(media_object_outcome(2, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        assert_eq!(reorder.pending_len(), 1);
        let first = reorder
            .push(media_object_outcome(1, 10, keyframe))
            .unwrap()
            .unwrap();
        assert_eq!(completed_object_index(&first), Some(1));
        assert_eq!(
            completed_object_index(&reorder.take_next().unwrap().unwrap()),
            Some(2)
        );
        assert_eq!(reorder.pending_len(), 0);
    }

    #[test]
    fn explicit_discontinuity_keyframe_fast_forwards_bounded_reorder() {
        let barrier = FrameFlags::KEYFRAME
            .union(FrameFlags::CODEC_CONFIG)
            .union(FrameFlags::DISCONTINUITY);
        let mut reorder = MediaObjectReorder::new(1);

        assert!(
            reorder
                .push(media_object_outcome(2, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        let recovered = reorder
            .push(media_object_outcome(3, 20, barrier))
            .unwrap()
            .unwrap();
        assert_eq!(completed_object_index(&recovered), Some(3));
        assert_eq!(reorder.pending_len(), 0);
        assert_eq!(
            completed_object_index(
                &reorder
                    .push(media_object_outcome(1, 10, FrameFlags::NONE))
                    .unwrap()
                    .unwrap()
            ),
            Some(1)
        );
    }

    #[test]
    fn malformed_media_object_bypasses_reorder_and_remains_terminal() {
        let mut reorder = MediaObjectReorder::new(1);
        assert!(
            reorder
                .push(media_object_outcome(2, 11, FrameFlags::NONE))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            reorder
                .push(MediaObjectReadOutcome::Malformed("bad object".into()))
                .unwrap(),
            Some(MediaObjectReadOutcome::Malformed(_))
        ));
        assert_eq!(reorder.pending_len(), 1);
    }

    #[test]
    fn media_objects_begin_on_a_configured_keyframe_then_deliver_contiguously() {
        let keyframe_flags = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequence::new();

        assert_eq!(
            sequence.classify(1, &media_object_frame(1, FrameFlags::NONE)),
            MediaObjectSequenceDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(2, &media_object_frame(2, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: true
            }
        );
        assert_eq!(
            sequence.classify(3, &media_object_frame(3, FrameFlags::NONE)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: false
            }
        );
    }

    #[test]
    fn media_object_sequence_gaps_drop_deltas_until_a_new_keyframe() {
        let keyframe_flags = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequence::new();

        assert!(matches!(
            sequence.classify(1, &media_object_frame(10, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver { .. }
        ));
        assert_eq!(
            sequence.classify(3, &media_object_frame(12, FrameFlags::NONE)),
            MediaObjectSequenceDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(4, &media_object_frame(13, FrameFlags::NONE)),
            MediaObjectSequenceDecision::DropUntilKeyframe
        );
        assert_eq!(
            sequence.classify(5, &media_object_frame(14, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: true
            }
        );
    }

    #[test]
    fn late_media_object_completion_cannot_rewind_a_recovered_stream() {
        let keyframe_flags = FrameFlags::KEYFRAME.union(FrameFlags::CODEC_CONFIG);
        let mut sequence = MediaObjectSequence::new();

        assert!(matches!(
            sequence.classify(10, &media_object_frame(10, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver { .. }
        ));
        assert!(sequence.note_dropped_object(12));
        assert_eq!(
            sequence.classify(13, &media_object_frame(13, keyframe_flags)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: true
            }
        );
        assert!(!sequence.note_dropped_object(11));
        assert_eq!(
            sequence.classify(12, &media_object_frame(12, FrameFlags::NONE)),
            MediaObjectSequenceDecision::DropLate
        );
        assert_eq!(
            sequence.classify(14, &media_object_frame(14, FrameFlags::NONE)),
            MediaObjectSequenceDecision::Deliver {
                discontinuity: false
            }
        );
    }
}
