use std::time::Duration;

use sigil_protocol::{
    KeyframeRequestReasonV3, MediaControlRequestV3, write_media_control_request_v3,
};

use crate::commands::state::{AppState, MediaControlRequestSender};

const MEDIA_CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) async fn run_media_control_writer_v3(
    mut stream: iroh::endpoint::SendStream,
    mut requests: tokio::sync::mpsc::Receiver<(KeyframeRequestReasonV3, Option<u64>)>,
) {
    let mut request_id = 0_u64;
    while let Some((reason, last_sequence)) = requests.recv().await {
        let Some(next_request_id) = request_id.checked_add(1) else {
            eprintln!("[client] media v3 keyframe request id overflowed");
            break;
        };
        request_id = next_request_id;
        let request = MediaControlRequestV3::request_keyframe(request_id, last_sequence, reason);
        match tokio::time::timeout(
            MEDIA_CONTROL_WRITE_TIMEOUT,
            write_media_control_request_v3(&mut stream, &request),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("[client] media v3 keyframe request failed: {error}");
                break;
            }
            Err(_) => {
                eprintln!("[client] media v3 keyframe request timed out");
                break;
            }
        }
    }
    let _ = stream.finish();
}

fn parse_keyframe_request_reason(reason: &str) -> Result<KeyframeRequestReasonV3, String> {
    match reason {
        "join" => Ok(KeyframeRequestReasonV3::Join),
        "transport-gap" => Ok(KeyframeRequestReasonV3::TransportGap),
        "delivery-timeout" => Ok(KeyframeRequestReasonV3::DeliveryTimeout),
        "discontinuity" | "decoder-reset" | "decoder-error" => {
            Ok(KeyframeRequestReasonV3::DecoderReset)
        }
        "frontend-backpressure" => Ok(KeyframeRequestReasonV3::FrontendBackpressure),
        _ => Err(format!("Unsupported keyframe request reason: {reason}")),
    }
}

pub(crate) fn try_queue_media_keyframe_request(
    sender: Option<&MediaControlRequestSender>,
    reason: KeyframeRequestReasonV3,
    last_sequence: Option<u64>,
) {
    if let Some(sender) = sender {
        let _ = sender.try_send((reason, last_sequence));
    }
}

pub(crate) async fn request_keyframe(
    state: &AppState,
    generation: u64,
    reason: String,
) -> Result<bool, String> {
    let reason = parse_keyframe_request_reason(&reason)?;
    let control = state.media_control.lock().await;
    let Some((current_generation, sender)) = control.as_ref() else {
        return Ok(false);
    };
    if *current_generation != generation {
        return Ok(false);
    }
    match sender.try_send((reason, None)) {
        Ok(()) => Ok(true),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err("Media control channel closed".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframe_request_reason_mapping_is_strict_and_coalesces_decoder_reasons() {
        assert_eq!(
            parse_keyframe_request_reason("transport-gap").unwrap(),
            KeyframeRequestReasonV3::TransportGap
        );
        assert_eq!(
            parse_keyframe_request_reason("discontinuity").unwrap(),
            KeyframeRequestReasonV3::DecoderReset
        );
        assert_eq!(
            parse_keyframe_request_reason("decoder-error").unwrap(),
            KeyframeRequestReasonV3::DecoderReset
        );
        assert!(parse_keyframe_request_reason("please").is_err());
    }
}
