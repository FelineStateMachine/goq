use super::state::AppState;
pub use crate::media::adaptive_feedback::ClientMediaFeedbackReport;
use crate::media::adaptive_feedback::send_media_feedback;
use crate::media::audio_delivery;
pub use crate::media::connect::ConnectResult;
use crate::media::connect::{connect_client, disconnect_client};
use crate::media::frame_channel::acknowledge_frame_delivery;
use crate::media::input_delivery::PointerFeedbackPayload;
use crate::media::input_delivery::send_input;
use crate::media::media_control::request_keyframe;
use sigil_protocol::InputEvent;
use tauri::{
    AppHandle, State,
    ipc::{Channel, Response},
};

// ─── Client commands ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn iroh_client_connect(
    app: AppHandle,
    state: State<'_, AppState>,
    pin: String,
    frame_channel: Channel<Response>,
    audio_channel: Channel<Response>,
    pointer_channel: Channel<PointerFeedbackPayload>,
    audio_supported: bool,
) -> Result<ConnectResult, String> {
    connect_client(
        app,
        &state,
        pin,
        frame_channel,
        audio_channel,
        pointer_channel,
        audio_supported,
    )
    .await
}

#[tauri::command]
pub async fn iroh_client_send_input(
    state: State<'_, AppState>,
    event: InputEvent,
) -> Result<bool, String> {
    send_input(&state, event).await
}

#[tauri::command]
pub async fn iroh_client_request_keyframe(
    state: State<'_, AppState>,
    generation: u64,
    reason: String,
) -> Result<bool, String> {
    request_keyframe(&state, generation, reason).await
}

#[tauri::command]
pub async fn iroh_client_send_media_feedback(
    state: State<'_, AppState>,
    generation: u64,
    report: ClientMediaFeedbackReport,
) -> Result<bool, String> {
    send_media_feedback(&state, generation, report).await
}

#[tauri::command]
pub async fn iroh_client_ack_frame(
    state: State<'_, AppState>,
    generation: u64,
) -> Result<bool, String> {
    acknowledge_frame_delivery(&state, generation).await
}

#[tauri::command]
pub fn iroh_client_ack_audio(
    state: State<'_, AppState>,
    generation: u64,
    delivery_id: u64,
) -> Result<bool, String> {
    audio_delivery::iroh_client_ack_audio(state, generation, delivery_id)
}

#[tauri::command]
pub async fn iroh_client_stop_audio(
    state: State<'_, AppState>,
    expected_generation: u64,
) -> Result<bool, String> {
    audio_delivery::iroh_client_stop_audio(state, expected_generation).await
}

#[tauri::command]
pub async fn iroh_client_disconnect(state: State<'_, AppState>) -> Result<bool, String> {
    disconnect_client(&state).await
}
