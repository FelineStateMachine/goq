use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{ProtocolError, Result};

pub(crate) async fn write_json<T, W>(writer: &mut W, value: &T, maximum: usize) -> Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value)?;
    validate_message_length(payload.len(), maximum)?;

    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub(crate) async fn read_json<T, R>(reader: &mut R, maximum: usize) -> Result<Option<T>>
where
    T: DeserializeOwned,
    R: AsyncRead + Unpin,
{
    let Some(length_bytes) = read_prefix_or_eof(reader).await? else {
        return Ok(None);
    };
    let length = u32::from_be_bytes(length_bytes) as usize;
    validate_message_length(length, maximum)?;

    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    Ok(Some(serde_json::from_slice(&payload)?))
}

pub(crate) async fn read_prefix_or_eof<R>(reader: &mut R) -> Result<Option<[u8; 4]>>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0; 4];
    let read = reader.read(&mut prefix[..1]).await?;
    if read == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut prefix[1..]).await?;
    Ok(Some(prefix))
}

fn validate_message_length(actual: usize, maximum: usize) -> Result<()> {
    if actual == 0 || actual > maximum || actual > u32::MAX as usize {
        return Err(ProtocolError::InvalidMessageLength { actual, maximum });
    }
    Ok(())
}
