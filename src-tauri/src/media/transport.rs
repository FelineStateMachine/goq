use std::time::Duration;

use iroh::Endpoint;
use sigil_protocol::{
    CONTROL_ALPN_V1, CONTROL_ALPN_V2, Capability, ClientHello, ClientHelloV2, MEDIA_ALPN_V3,
    PointerSurfaceDimensions, SessionSnapshotV2, read_host_hello, read_host_hello_v2,
    write_client_hello, write_client_hello_v2,
};

pub const MEDIA_TRANSPORT_NAMES: [&str; 2] = ["iroh-moq", "grouped-v3"];

pub(crate) const CLIENT_ENDPOINT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct NegotiatedV1Stream {
    pub(crate) session_id: u64,
    pub(crate) capabilities: Vec<Capability>,
    pub(crate) pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
}

pub(crate) struct NegotiatedV2Stream {
    pub(crate) session_id: u64,
    pub(crate) pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
    pub(crate) initial_snapshot: SessionSnapshotV2,
    pub(crate) media_subscription_capability: String,
}

pub(crate) enum NegotiatedMediaStream {
    V2(NegotiatedV2Stream),
    V1(NegotiatedV1Stream),
}

impl NegotiatedMediaStream {
    pub(crate) fn session_id(&self) -> u64 {
        match self {
            Self::V2(stream) => stream.session_id,
            Self::V1(stream) => stream.session_id,
        }
    }

    pub(crate) fn pointer_surface_dimensions(&self) -> Option<PointerSurfaceDimensions> {
        match self {
            Self::V2(stream) => stream.pointer_surface_dimensions,
            Self::V1(stream) => stream.pointer_surface_dimensions,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlProtocol {
    V2,
    V1,
}

impl ControlProtocol {
    pub(crate) const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::V2 => "control-v2",
            Self::V1 => "legacy-v1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaTransport {
    UpstreamMoq,
    GroupedObjectsV3,
}

impl MediaTransport {
    pub(crate) const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::UpstreamMoq => MEDIA_TRANSPORT_NAMES[0],
            Self::GroupedObjectsV3 => MEDIA_TRANSPORT_NAMES[1],
        }
    }
}

pub(crate) async fn negotiate_v1(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    nonce: [u8; 16],
    capabilities: Vec<Capability>,
    required: Option<Capability>,
    stream_name: &str,
    invitation: Option<&str>,
) -> Result<NegotiatedV1Stream, String> {
    let mut hello = ClientHello::new("portal/0.1.0", nonce, capabilities.clone());
    if let Some(invitation) = invitation {
        hello = hello.with_invitation(invitation);
    }
    write_client_hello(send, &hello)
        .await
        .map_err(|e| format!("Failed to send {stream_name} handshake: {e}"))?;
    let response = tokio::time::timeout(Duration::from_secs(10), read_host_hello(recv))
        .await
        .map_err(|_| format!("Timed out waiting for {stream_name} handshake"))?
        .map_err(|e| format!("Invalid {stream_name} handshake: {e}"))?
        .ok_or_else(|| format!("Host closed during {stream_name} handshake"))?;
    if !response.accepted {
        return Err(format!(
            "Host rejected {stream_name} stream: {}",
            response.message.as_deref().unwrap_or("unspecified reason")
        ));
    }
    if let Some(required) = required
        && !response.capabilities.contains(&required)
    {
        return Err(format!(
            "Host accepted {stream_name} without required capability {required:?}"
        ));
    }
    if let Some(unoffered) = response
        .capabilities
        .iter()
        .find(|capability| !capabilities.contains(capability))
    {
        return Err(format!(
            "Host accepted unoffered {stream_name} capability {unoffered:?}"
        ));
    }
    let session_id = response
        .session_id
        .ok_or_else(|| format!("Host omitted {stream_name} session ID"))?;
    Ok(NegotiatedV1Stream {
        session_id,
        capabilities: response.capabilities,
        pointer_surface_dimensions: response.pointer_surface_dimensions,
    })
}

async fn negotiate_v2(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    nonce: [u8; 16],
    invitation: Option<&str>,
) -> Result<NegotiatedV2Stream, String> {
    let capabilities = vec![Capability::VideoH264];
    let mut hello = ClientHelloV2::new("portal/0.1.0", nonce, capabilities.clone());
    if let Some(invitation) = invitation {
        hello = hello.with_invitation(invitation);
    }
    write_client_hello_v2(send, &hello)
        .await
        .map_err(|error| format!("Failed to send control v2 handshake: {error}"))?;
    let response = tokio::time::timeout(Duration::from_secs(10), read_host_hello_v2(recv))
        .await
        .map_err(|_| "Timed out waiting for control v2 handshake".to_string())?
        .map_err(|error| format!("Invalid control v2 handshake: {error}"))?
        .ok_or_else(|| "Host closed during control v2 handshake".to_string())?;
    if !response.accepted {
        return Err(format!(
            "Host rejected control v2 stream: {}",
            response.message.as_deref().unwrap_or("unspecified reason")
        ));
    }
    if !response.capabilities.contains(&Capability::VideoH264) {
        return Err("Host accepted control v2 without required VideoH264 capability".to_string());
    }
    if let Some(unoffered) = response
        .capabilities
        .iter()
        .find(|capability| !capabilities.contains(capability))
    {
        return Err(format!(
            "Host accepted unoffered control v2 capability {unoffered:?}"
        ));
    }
    Ok(NegotiatedV2Stream {
        session_id: response
            .session_id
            .ok_or_else(|| "Host omitted control v2 session ID".to_string())?,
        pointer_surface_dimensions: response.pointer_surface_dimensions,
        initial_snapshot: response
            .snapshot
            .ok_or_else(|| "Host omitted initial control v2 snapshot".to_string())?,
        media_subscription_capability: response
            .media_subscription_capability
            .ok_or_else(|| "Host omitted control v2 media subscription capability".to_string())?,
    })
}

fn connection_error_is_unsupported_alpn(error: &iroh::endpoint::ConnectionError) -> bool {
    matches!(
        error,
        iroh::endpoint::ConnectionError::ConnectionClosed(close)
            if close.error_code == iroh::endpoint::TransportErrorCode::crypto(0x78)
    )
}

pub(crate) fn connect_error_is_unsupported_alpn(error: &iroh::endpoint::ConnectError) -> bool {
    match error {
        iroh::endpoint::ConnectError::Connecting {
            source: iroh::endpoint::ConnectingError::ConnectionError { source, .. },
            ..
        }
        | iroh::endpoint::ConnectError::Connection { source, .. } => {
            connection_error_is_unsupported_alpn(source)
        }
        _ => false,
    }
}

pub(crate) async fn open_negotiated_media_stream(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
    invitation: Option<&str>,
) -> Result<
    (
        iroh::endpoint::Connection,
        iroh::endpoint::RecvStream,
        iroh::endpoint::SendStream,
        NegotiatedMediaStream,
        MediaTransport,
        ControlProtocol,
    ),
    String,
> {
    match endpoint.connect(address.clone(), CONTROL_ALPN_V2).await {
        Ok(connection) => {
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|error| format!("Failed to open control v2 handshake: {error}"))?;
            let negotiation = negotiate_v2(&mut send, &mut recv, nonce, invitation).await?;
            Ok((
                connection,
                recv,
                send,
                NegotiatedMediaStream::V2(negotiation),
                MediaTransport::UpstreamMoq,
                ControlProtocol::V2,
            ))
        }
        Err(v2_error) if connect_error_is_unsupported_alpn(&v2_error) => {
            open_negotiated_v1_or_v3(endpoint, address, nonce, invitation, v2_error).await
        }
        Err(v2_error) => Err(format!(
            "Control v2 connection failed without an explicit unsupported-ALPN signal; refusing an unsafe downgrade: {v2_error}"
        )),
    }
}

async fn open_negotiated_v1_or_v3(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
    invitation: Option<&str>,
    v2_error: iroh::endpoint::ConnectError,
) -> Result<
    (
        iroh::endpoint::Connection,
        iroh::endpoint::RecvStream,
        iroh::endpoint::SendStream,
        NegotiatedMediaStream,
        MediaTransport,
        ControlProtocol,
    ),
    String,
> {
    match endpoint.connect(address.clone(), CONTROL_ALPN_V1).await {
        Ok(connection) => {
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|error| format!("Failed to open control handshake: {error}"))?;
            let negotiation = negotiate_v1(
                &mut send,
                &mut recv,
                nonce,
                vec![Capability::VideoH264],
                Some(Capability::VideoH264),
                "control",
                invitation,
            )
            .await?;
            // CONTROL owns the authenticated host lease. Keep both the
            // connection and the client->host send leg alive for keyframe
            // requests while media uses a separate upstream MoQ session.
            Ok((
                connection,
                recv,
                send,
                NegotiatedMediaStream::V1(negotiation),
                MediaTransport::UpstreamMoq,
                ControlProtocol::V1,
            ))
        }
        Err(control_error) if connect_error_is_unsupported_alpn(&control_error) => {
            // Grouped-v3 is the sole compatibility tier below upstream MoQ.
            let connection = endpoint
                .connect(address.clone(), MEDIA_ALPN_V3)
                .await
                .map_err(|v3_error| {
                    format!(
                        "Control v2 was unsupported ({v2_error}); control v1 failed ({control_error}); media v3 compatibility connection also failed ({v3_error})"
                    )
                })?;
            let (mut send, mut recv) = connection
                .open_bi()
                .await
                .map_err(|error| format!("Failed to open media v3 handshake: {error}"))?;
            let negotiation = negotiate_v1(
                &mut send,
                &mut recv,
                nonce,
                vec![Capability::VideoH264],
                Some(Capability::VideoH264),
                "media v3",
                invitation,
            )
            .await?;
            Ok((
                connection,
                recv,
                send,
                NegotiatedMediaStream::V1(negotiation),
                MediaTransport::GroupedObjectsV3,
                ControlProtocol::V1,
            ))
        }
        Err(control_error) => Err(format!(
            "Control v2 was unsupported ({v2_error}); control v1 failed without an explicit unsupported-ALPN signal; refusing an unsafe media downgrade: {control_error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_moq_transport_is_distinct_from_legacy_compatibility() {
        assert_eq!(MediaTransport::UpstreamMoq.diagnostic_name(), "iroh-moq");
        assert_ne!(
            MediaTransport::UpstreamMoq,
            MediaTransport::GroupedObjectsV3
        );
    }

    #[test]
    fn compatibility_downgrade_requires_tls_no_application_protocol() {
        let unsupported = iroh::endpoint::ConnectionError::ConnectionClosed(
            iroh::endpoint::TransportError::new(
                iroh::endpoint::TransportErrorCode::crypto(0x78),
                "no application protocol".to_string(),
            )
            .into(),
        );
        assert!(connection_error_is_unsupported_alpn(&unsupported));
        assert!(!connection_error_is_unsupported_alpn(
            &iroh::endpoint::ConnectionError::TimedOut
        ));
        assert!(!connection_error_is_unsupported_alpn(
            &iroh::endpoint::ConnectionError::Reset
        ));
    }

    #[test]
    fn media_transport_names_match_diagnostic_mapping() {
        assert_eq!(MEDIA_TRANSPORT_NAMES, ["iroh-moq", "grouped-v3"]);
        assert_eq!(
            [
                MediaTransport::UpstreamMoq.diagnostic_name(),
                MediaTransport::GroupedObjectsV3.diagnostic_name(),
            ],
            MEDIA_TRANSPORT_NAMES,
        );
    }
}
