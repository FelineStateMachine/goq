// ─── H.264 Annex B → AVCC conversion ──────────────────────────────────────
// Sigil's negotiated media contract is H.264-only. The encoder emits Annex B
// start codes; WebCodecs expects length-prefixed NALs plus an AVC description.

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
