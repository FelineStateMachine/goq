// ─── Annex B → AVCC/hvcC/av1C conversion ──────────────────────────────────
// ffmpeg outputs Annex B (00 00 00 01 start codes) for H.264/H.265.
// WebCodecs expects length-prefixed NALs + description record.
// AV1 uses OBU format directly.

export function parseAnnexBNals(data) {
  const nals = [];
  let i = 0;
  while (i < data.length) {
    let scl = 0;
    if (i + 3 < data.length && data[i] === 0 && data[i+1] === 0 && data[i+2] === 0 && data[i+3] === 1) {
      scl = 4;
    } else if (i + 2 < data.length && data[i] === 0 && data[i+1] === 0 && data[i+2] === 1) {
      scl = 3;
    } else {
      i++; continue;
    }
    const nalStart = i + scl;
    let j = nalStart + 1;
    while (j < data.length) {
      if (j + 3 < data.length && data[j] === 0 && data[j+1] === 0 && data[j+2] === 0 && data[j+3] === 1) break;
      if (j + 2 < data.length && data[j] === 0 && data[j+1] === 0 && data[j+2] === 1) break;
      j++;
    }
    nals.push(data.subarray(nalStart, j));
    i = j;
  }
  return nals;
}

export function nalsToLengthPrefixed(nals) {
  let total = 0;
  for (const nal of nals) total += 4 + nal.length;
  const result = new Uint8Array(total);
  let off = 0;
  for (const nal of nals) {
    const dv = new DataView(result.buffer, off, 4);
    dv.setUint32(0, nal.length, false);
    result.set(nal, off + 4);
    off += 4 + nal.length;
  }
  return result;
}

// ─── H.264 ─────────────────────────────────────────────────────────────────

export function h264NalType(nal) { return nal[0] & 0x1f; }

export function buildAvcDescription(sps, pps) {
  const buf = new Uint8Array(11 + sps.length + pps.length);
  let off = 0;
  buf[off++] = 1;              // configurationVersion
  buf[off++] = sps[1];         // profile_idc
  buf[off++] = sps[2];         // constraint_flags
  buf[off++] = sps[3];         // level_idc
  buf[off++] = 0xFF;           // lengthSizeMinusOne = 3
  buf[off++] = 0xE1;           // numOfSPS = 1
  buf[off++] = (sps.length >> 8) & 0xFF;
  buf[off++] = sps.length & 0xFF;
  buf.set(sps, off); off += sps.length;
  buf[off++] = 1;              // numOfPPS = 1
  buf[off++] = (pps.length >> 8) & 0xFF;
  buf[off++] = pps.length & 0xFF;
  buf.set(pps, off);
  return buf.buffer;
}

export function avcCodecStr(sps) {
  const p = sps[1].toString(16).padStart(2, '0');
  const c = sps[2].toString(16).padStart(2, '0');
  const l = sps[3].toString(16).padStart(2, '0');
  return `avc1.${p}${c}${l}`;
}

// ─── H.265 / HEVC ──────────────────────────────────────────────────────────

export function hevcNalType(nal) { return (nal[0] >> 1) & 0x3f; }

export function buildHvcDescription(vps, sps, pps) {
  // hvcC record format (ISO 14496-15 8.3.3.1.2)
  const buf = new Uint8Array(23 + 15 + vps.length + sps.length + pps.length);
  let off = 0;
  buf[off++] = 1;              // configurationVersion
  buf[off++] = sps[1] & 0x1f; // profile_space=0, tier=0, profile_idc from SPS
  buf[off++] = sps[2];         // general_profile_compatibility_flags[0..7]
  buf[off++] = sps[3];         // [8..15]
  buf[off++] = sps[4];         // [16..23]
  buf[off++] = sps[5];         // [24..31]
  for (let i = 0; i < 6; i++) buf[off++] = sps[6 + i] || 0; // constraint flags
  buf[off++] = sps[12];        // general_level_idc
  buf[off++] = 0xF0; buf[off++] = 0x00; // min_spatial_segmentation_idc
  buf[off++] = 0xFC;           // parallelismType
  buf[off++] = 0xFC | ((sps.length > 2 ? 0 : 0)); // chromaFormat
  buf[off++] = 0xF8;           // bitDepthLumaMinus8
  buf[off++] = 0xF8;           // bitDepthChromaMinus8
  buf[off++] = 0x00; buf[off++] = 0x00; // avgFrameRate
  buf[off++] = 0x0F;           // lengthSizeMinusOne=3
  buf[off++] = 3;              // numOfArrays: VPS, SPS, PPS

  const writeNal = (nalType, nal) => {
    buf[off++] = 0x80 | nalType; // array_completeness=1
    buf[off++] = 0;
    buf[off++] = 1;              // numNalus = 1
    buf[off++] = (nal.length >> 8) & 0xFF;
    buf[off++] = nal.length & 0xFF;
    buf.set(nal, off);
    off += nal.length;
  };
  writeNal(32, vps);
  writeNal(33, sps);
  writeNal(34, pps);

  return buf.buffer;
}

export function hevcCodecStr(sps) {
  const profileIdc = (sps[1] & 0x1f);
  const levelIdc = sps.length > 13 ? sps[13] : 0;
  return `hvc1.${profileIdc.toString(16).padStart(2, '0')}.4.L${levelIdc}.B0`;
}

// ─── AV1 ───────────────────────────────────────────────────────────────────

export function parseAv1Obus(data) {
  // AV1 low-overhead bitstream: each OBU is length-prefixed
  const obus = [];
  let i = 0;
  while (i < data.length) {
    if (i + 1 >= data.length) break;
    const header = data[i];
    const obuType = (header >> 3) & 0x0f;
    const hasExtension = (header & 0x04) !== 0;
    const hasSize = (header & 0x02) !== 0;
    let off = i + 1 + (hasExtension ? 1 : 0);
    if (!hasSize) {
      obus.push({ type: obuType, data: data.subarray(i, data.length) });
      break;
    }
    let size = 0, shift = 0, bytesConsumed = 0;
    for (let j = 0; j < 4 && off + j < data.length; j++) {
      const b = data[off + j];
      size |= (b & 0x7f) << shift;
      shift += 7;
      bytesConsumed++;
      if ((b & 0x80) === 0) break;
    }
    off += bytesConsumed;
    obus.push({ type: obuType, data: data.subarray(i, off + size) });
    i = off + size;
  }
  return obus;
}

export function av1CodecStr(sequenceHeader) {
  const obuData = sequenceHeader;
  let off = (obuData[0] & 0x04) !== 0 ? 2 : 1;
  while (off < obuData.length && (obuData[off] & 0x80) !== 0) off++;
  off++;
  const b0 = obuData[off] || 0;
  const profile = (b0 >> 5) & 0x07;
  const reduced = (b0 >> 3) & 0x01;
  const levelIdx = reduced ? (b0 >> 2) & 0x1f : 0;
  return `av01.${profile}.${levelIdx.toString().padStart(2, '0')}M.08`;
}

export function buildAv1Description(seqHeaderObu) {
  return seqHeaderObu.buffer.slice(seqHeaderObu.byteOffset, seqHeaderObu.byteOffset + seqHeaderObu.byteLength);
}
