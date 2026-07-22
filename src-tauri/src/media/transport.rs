use std::time::Duration;

use iroh::Endpoint;
#[cfg(test)]
use sigil_protocol::decode_media_frame_object;
use sigil_protocol::{
    CONTROL_ALPN_V1, Capability, ClientHello, MEDIA_ALPN_V1, MEDIA_ALPN_V2, MEDIA_ALPN_V3,
    PointerSurfaceDimensions, read_host_hello, write_client_hello,
};

pub const MEDIA_TRANSPORT_NAMES: [&str; 5] = [
    "iroh-moq",
    "reliable-v0",
    "reliable-v1",
    "independent-v2",
    "grouped-v3",
];

pub(crate) const CLIENT_ENDPOINT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct NegotiatedV1Stream {
    pub(crate) session_id: u64,
    pub(crate) capabilities: Vec<Capability>,
    pub(crate) pointer_surface_dimensions: Option<PointerSurfaceDimensions>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaTransport {
    UpstreamMoq,
    LegacyV0,
    ReliableStreamV1,
    IndependentObjectsV2,
    GroupedObjectsV3,
}

impl MediaTransport {
    pub(crate) const fn diagnostic_name(self) -> &'static str {
        match self {
            Self::UpstreamMoq => MEDIA_TRANSPORT_NAMES[0],
            Self::LegacyV0 => MEDIA_TRANSPORT_NAMES[1],
            Self::ReliableStreamV1 => MEDIA_TRANSPORT_NAMES[2],
            Self::IndependentObjectsV2 => MEDIA_TRANSPORT_NAMES[3],
            Self::GroupedObjectsV3 => MEDIA_TRANSPORT_NAMES[4],
        }
    }

    pub(crate) const fn supports_adaptive_feedback(self) -> bool {
        matches!(self, Self::UpstreamMoq | Self::GroupedObjectsV3)
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

async fn open_legacy_negotiated_media_stream(
    endpoint: &Endpoint,
    address: &iroh::EndpointAddr,
    nonce: [u8; 16],
    invitation: Option<&str>,
) -> Result<
    (
        iroh::endpoint::Connection,
        iroh::endpoint::RecvStream,
        Option<iroh::endpoint::SendStream>,
        NegotiatedV1Stream,
        MediaTransport,
    ),
    String,
> {
    match endpoint.connect(address.clone(), MEDIA_ALPN_V3).await {
        Ok(connection) => {
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
                Some(send),
                negotiation,
                MediaTransport::GroupedObjectsV3,
            ))
        }
        Err(v3_error) if connect_error_is_unsupported_alpn(&v3_error) => {
            match endpoint.connect(address.clone(), MEDIA_ALPN_V2).await {
                Ok(connection) => {
                    let (mut send, mut recv) = connection
                        .open_bi()
                        .await
                        .map_err(|error| format!("Failed to open media v2 handshake: {error}"))?;
                    let negotiation = negotiate_v1(
                        &mut send,
                        &mut recv,
                        nonce,
                        vec![Capability::VideoH264],
                        Some(Capability::VideoH264),
                        "media v2",
                        invitation,
                    )
                    .await?;
                    send.finish()
                        .map_err(|error| format!("Failed to finish media v2 handshake: {error}"))?;
                    Ok((
                        connection,
                        recv,
                        None,
                        negotiation,
                        MediaTransport::IndependentObjectsV2,
                    ))
                }
                Err(v2_error) if connect_error_is_unsupported_alpn(&v2_error) => {
                    let connection = endpoint
                    .connect(address.clone(), MEDIA_ALPN_V1)
                    .await
                    .map_err(|v1_error| {
                        format!(
                            "Failed to connect media v3 ({v3_error}); v2 compatibility connection failed ({v2_error}); v1 compatibility connection also failed ({v1_error})"
                        )
                    })?;
                    let (mut send, mut recv) = connection
                        .open_bi()
                        .await
                        .map_err(|error| format!("Failed to open media v1 stream: {error}"))?;
                    let negotiation = negotiate_v1(
                        &mut send,
                        &mut recv,
                        nonce,
                        vec![Capability::VideoH264],
                        Some(Capability::VideoH264),
                        "media v1",
                        invitation,
                    )
                    .await?;
                    send.finish()
                        .map_err(|error| format!("Failed to finish media v1 handshake: {error}"))?;
                    Ok((
                        connection,
                        recv,
                        None,
                        negotiation,
                        MediaTransport::ReliableStreamV1,
                    ))
                }
                Err(v2_error) => Err(format!(
                    "Media v2 compatibility connection failed without an explicit unsupported-ALPN signal; refusing an unsafe downgrade to v1: {v2_error}"
                )),
            }
        }
        Err(v3_error) => Err(format!(
            "Media v3 connection failed without an explicit unsupported-ALPN signal; refusing an unsafe compatibility downgrade: {v3_error}"
        )),
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
        Option<iroh::endpoint::SendStream>,
        NegotiatedV1Stream,
        MediaTransport,
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
                Some(send),
                negotiation,
                MediaTransport::UpstreamMoq,
            ))
        }
        Err(control_error) if connect_error_is_unsupported_alpn(&control_error) => {
            open_legacy_negotiated_media_stream(endpoint, address, nonce, invitation).await
        }
        Err(control_error) => Err(format!(
            "Control connection failed without an explicit unsupported-ALPN signal; refusing an unsafe media downgrade: {control_error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_moq_transport_is_distinct_from_legacy_compatibility() {
        assert_eq!(MediaTransport::UpstreamMoq.diagnostic_name(), "iroh-moq");
        assert!(MediaTransport::UpstreamMoq.supports_adaptive_feedback());
        assert!(MediaTransport::GroupedObjectsV3.supports_adaptive_feedback());
        assert!(!MediaTransport::IndependentObjectsV2.supports_adaptive_feedback());
        assert_ne!(
            MediaTransport::UpstreamMoq,
            MediaTransport::GroupedObjectsV3
        );
        assert!(decode_media_frame_object(&[0_u8; 8]).is_err());
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
        assert_eq!(
            MEDIA_TRANSPORT_NAMES,
            [
                "iroh-moq",
                "reliable-v0",
                "reliable-v1",
                "independent-v2",
                "grouped-v3",
            ]
        );
        assert_eq!(
            [
                MediaTransport::UpstreamMoq.diagnostic_name(),
                MediaTransport::LegacyV0.diagnostic_name(),
                MediaTransport::ReliableStreamV1.diagnostic_name(),
                MediaTransport::IndependentObjectsV2.diagnostic_name(),
                MediaTransport::GroupedObjectsV3.diagnostic_name(),
            ],
            MEDIA_TRANSPORT_NAMES,
        );
    }
}
