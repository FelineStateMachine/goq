use anyhow::{Result, ensure};

use super::MAX_ENCODE_BUFFER;

#[derive(Default)]
pub(super) struct AnnexBAccessUnitParser {
    buffer: Vec<u8>,
}

impl AnnexBAccessUnitParser {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(bytes);
        ensure!(
            self.buffer.len() <= MAX_ENCODE_BUFFER,
            "encoded stream exceeded {MAX_ENCODE_BUFFER} bytes without a frame boundary"
        );

        let mut frames = Vec::new();
        while let Some(first) = find_h264_aud(&self.buffer, 0) {
            if first > 0 {
                self.buffer.drain(..first);
            }
            let Some(next) = find_h264_aud(&self.buffer, 4) else {
                break;
            };
            let frame: Vec<u8> = self.buffer.drain(..next).collect();
            if !frame.is_empty() {
                frames.push(frame);
            }
        }
        Ok(frames)
    }
}

fn find_h264_aud(data: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index + 4 <= data.len() {
        let nal_offset = if data.get(index..index + 4) == Some(&[0, 0, 0, 1]) {
            Some(index + 4)
        } else if data.get(index..index + 3) == Some(&[0, 0, 1]) {
            Some(index + 3)
        } else {
            None
        };
        if let Some(nal_offset) = nal_offset
            && data.get(nal_offset).is_some_and(|byte| byte & 0x1f == 9)
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_access_units_on_aud() {
        let first = [0, 0, 0, 1, 9, 0x10, 0, 0, 1, 5, 1, 2, 3];
        let second = [0, 0, 0, 1, 9, 0x10, 0, 0, 1, 1, 4, 5];
        let third = [0, 0, 1, 9, 0x10, 0, 0, 1, 1, 6, 7];
        let mut parser = AnnexBAccessUnitParser::default();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&first);
        bytes.extend_from_slice(&second);
        bytes.extend_from_slice(&third);
        let frames = parser.push(&bytes).unwrap();

        assert_eq!(frames, vec![first.to_vec(), second.to_vec()]);
    }
}
