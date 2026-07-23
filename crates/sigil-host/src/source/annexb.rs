use anyhow::{Result, ensure};

use super::MAX_ENCODE_BUFFER;

#[derive(Default)]
pub(super) struct AnnexBAccessUnitParser {
    buffer: Vec<u8>,
    next_aud_scan: usize,
    has_access_unit_start: bool,
    #[cfg(test)]
    scanned_candidates: usize,
}

impl AnnexBAccessUnitParser {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let encoded_len = self.buffer.len().checked_add(bytes.len());
        ensure!(
            encoded_len.is_some_and(|len| len <= MAX_ENCODE_BUFFER),
            "encoded stream exceeded {MAX_ENCODE_BUFFER} bytes without a frame boundary"
        );
        self.buffer.extend_from_slice(bytes);

        let mut frames = Vec::new();
        loop {
            let scan = find_h264_aud(&self.buffer, self.next_aud_scan);
            #[cfg(test)]
            {
                self.scanned_candidates += scan.examined;
            }
            let Some(aud) = scan.found else {
                self.next_aud_scan = scan.resume_at;
                break;
            };

            if !self.has_access_unit_start {
                if aud > 0 {
                    self.buffer.drain(..aud);
                }
                self.has_access_unit_start = true;
                // Preserve the original parser's search semantics: once the
                // first AUD is at offset zero, look for its successor at
                // offset four. That is after a three-byte AUD's NAL header and
                // at a four-byte AUD's NAL header.
                self.next_aud_scan = 4;
                continue;
            }

            let frame: Vec<u8> = self.buffer.drain(..aud).collect();
            if !frame.is_empty() {
                frames.push(frame);
            }
            // The AUD that ended the emitted access unit is now at offset
            // zero and begins the next one.
            self.next_aud_scan = 4;
        }
        Ok(frames)
    }
}

struct AudScan {
    found: Option<usize>,
    resume_at: usize,
    #[cfg(test)]
    examined: usize,
}

fn find_h264_aud(data: &[u8], start: usize) -> AudScan {
    let mut index = start;
    #[cfg(test)]
    let mut examined = 0;
    while index + 4 <= data.len() {
        #[cfg(test)]
        {
            examined += 1;
        }
        let nal_offset = if data.get(index..index + 4) == Some(&[0, 0, 0, 1]) {
            // The start code is complete, but its NAL header may arrive in the
            // next chunk. Revisit this one candidate rather than skipping it.
            if index + 4 == data.len() {
                break;
            }
            Some(index + 4)
        } else if data.get(index..index + 3) == Some(&[0, 0, 1]) {
            Some(index + 3)
        } else {
            None
        };
        if let Some(nal_offset) = nal_offset
            && data.get(nal_offset).is_some_and(|byte| byte & 0x1f == 9)
        {
            return AudScan {
                found: Some(index),
                resume_at: index,
                #[cfg(test)]
                examined,
            };
        }
        index += 1;
    }
    AudScan {
        found: None,
        resume_at: index,
        #[cfg(test)]
        examined,
    }
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

    #[test]
    fn recognizes_three_and_four_byte_start_codes_split_at_every_boundary() {
        let first = [0, 0, 0, 1, 9, 0x10, 0, 0, 1, 5, 1, 2, 3];
        let second = [0, 0, 1, 9, 0x10, 0, 0, 1, 1, 4, 5];
        let third = [0, 0, 0, 1, 9, 0x10, 0, 0, 1, 1, 6, 7];
        let stream = [first.as_slice(), second.as_slice(), third.as_slice()].concat();

        for split in 1..stream.len() {
            let mut parser = AnnexBAccessUnitParser::default();
            let mut frames = parser.push(&stream[..split]).unwrap();
            frames.extend(parser.push(&stream[split..]).unwrap());

            assert_eq!(
                frames,
                vec![first.to_vec(), second.to_vec()],
                "split at byte {split}"
            );
        }

        let mut bytewise = AnnexBAccessUnitParser::default();
        let frames: Vec<Vec<u8>> = stream
            .chunks(1)
            .flat_map(|chunk| bytewise.push(chunk).unwrap())
            .collect();
        assert_eq!(frames, vec![first.to_vec(), second.to_vec()]);
    }

    #[test]
    fn scans_large_chunked_access_unit_in_linear_work() {
        const PAYLOAD_LEN: usize = 1024 * 1024;
        const CHUNK_LEN: usize = 257;

        let mut first = vec![0, 0, 0, 1, 9, 0x10, 0, 0, 1, 5];
        first.resize(first.len() + PAYLOAD_LEN, 0x55);
        let second = [0, 0, 1, 9, 0x10, 0, 0, 1, 1, 4, 5];
        let third = [0, 0, 0, 1, 9, 0x10];
        let stream = [first.as_slice(), second.as_slice(), third.as_slice()].concat();
        let mut parser = AnnexBAccessUnitParser::default();
        let mut frames = Vec::new();

        for chunk in stream.chunks(CHUNK_LEN) {
            let before = parser.scanned_candidates;
            frames.extend(parser.push(chunk).unwrap());
            let work = parser.scanned_candidates - before;
            assert!(
                work <= chunk.len() + 8,
                "one {chunk_len}-byte append examined {work} candidates",
                chunk_len = chunk.len()
            );
        }

        assert_eq!(frames, vec![first, second.to_vec()]);
        assert!(
            parser.scanned_candidates <= stream.len() + 4 * stream.len().div_ceil(CHUNK_LEN),
            "{} bytes caused {} candidate scans",
            stream.len(),
            parser.scanned_candidates
        );
    }

    #[test]
    fn resets_scan_offset_after_emitting_each_access_unit() {
        let leading_junk = [0x44; 19];
        let first = [0, 0, 1, 9, 0x10, 0, 0, 1, 5, 1];
        let second = [0, 0, 0, 1, 9, 0x10, 0, 0, 1, 1, 2];
        let third = [0, 0, 1, 9, 0x10, 0, 0, 1, 1, 3];
        let fourth = [0, 0, 0, 1, 9, 0x10];
        let stream = [
            leading_junk.as_slice(),
            first.as_slice(),
            second.as_slice(),
            third.as_slice(),
            fourth.as_slice(),
        ]
        .concat();
        let mut parser = AnnexBAccessUnitParser::default();
        let frames: Vec<Vec<u8>> = stream
            .chunks(7)
            .flat_map(|chunk| parser.push(chunk).unwrap())
            .collect();

        assert_eq!(
            frames,
            vec![first.to_vec(), second.to_vec(), third.to_vec()]
        );
        assert!(parser.has_access_unit_start);
        assert!(parser.next_aud_scan >= 4);
    }

    #[test]
    fn oversized_append_is_rejected_without_allocating_or_poisoning_state() {
        let first = [0, 0, 0, 1, 9, 0x10, 0, 0, 1, 5, 1, 2, 3];
        let second = [0, 0, 1, 9, 0x10, 0, 0, 1, 1, 4, 5];
        let mut parser = AnnexBAccessUnitParser::default();
        assert!(parser.push(&first).unwrap().is_empty());
        let buffer_len = parser.buffer.len();
        let scan_offset = parser.next_aud_scan;
        let scan_work = parser.scanned_candidates;

        let oversized = vec![0x55; MAX_ENCODE_BUFFER];
        let error = parser.push(&oversized).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("encoded stream exceeded 16777216 bytes")
        );
        assert_eq!(parser.buffer.len(), buffer_len);
        assert_eq!(parser.next_aud_scan, scan_offset);
        assert_eq!(parser.scanned_candidates, scan_work);

        assert_eq!(parser.push(&second).unwrap(), vec![first.to_vec()]);
    }
}
