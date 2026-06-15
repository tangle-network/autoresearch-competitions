//! Operator registration payload for the autoresearch-competitions blueprint.
//!
//! When an operator runs the binary in registration mode (the SDK's
//! preregistration flow), the runner writes a TLV (Type-Length-Value) payload to
//! the path the Blueprint Manager polls, then forwards the bytes to the on-chain
//! manager's `onRegister` hook. This module builds that payload.
//!
//! The encoding mirrors the proven trading-blueprint TLV layout: a one-byte type,
//! a big-endian `u16` length, then the value. Fields are length-prefixed so the
//! manager can parse forward without ambiguity, and optional fields are simply
//! omitted (never emitted with a zero length sentinel that a reader might confuse
//! with empty content).

/// TLV type: referee max capacity — how many competitions this operator will
/// adjudicate concurrently (`u32` big-endian, 4 bytes).
const TLV_MAX_CAPACITY: u8 = 0x01;
/// TLV type: operator API endpoint where the off-chain Referee/scoring service is
/// reachable (UTF-8 string).
const TLV_API_ENDPOINT: u8 = 0x02;
/// TLV type: supported scorer kinds, comma-separated, mirroring
/// `autoresearch_runtime::types::ScorerKind` discriminants — e.g.
/// `"held_out_eval,private_oracle"` (UTF-8 string).
const TLV_SUPPORTED_SCORERS: u8 = 0x03;

fn write_tlv(buf: &mut Vec<u8>, tlv_type: u8, value: &[u8]) {
    buf.push(tlv_type);
    let len = u16::try_from(value.len()).unwrap_or(u16::MAX);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(value);
}

/// Build a TLV-encoded registration payload for an autoresearch-competitions
/// operator (a Referee/scoring node).
///
/// `max_capacity` is always emitted. `api_endpoint` and `supported_scorers` are
/// emitted only when non-empty, so a minimal operator registers with capacity
/// alone.
#[must_use]
pub fn competitions_registration_payload(
    max_capacity: u32,
    api_endpoint: &str,
    supported_scorers: &str,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16 + api_endpoint.len() + supported_scorers.len());
    write_tlv(&mut payload, TLV_MAX_CAPACITY, &max_capacity.to_be_bytes());
    if !api_endpoint.is_empty() {
        write_tlv(&mut payload, TLV_API_ENDPOINT, api_endpoint.as_bytes());
    }
    if !supported_scorers.is_empty() {
        write_tlv(
            &mut payload,
            TLV_SUPPORTED_SCORERS,
            supported_scorers.as_bytes(),
        );
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_full_payload() {
        let payload = competitions_registration_payload(
            8,
            "http://localhost:9300",
            "held_out_eval,private_oracle",
        );

        let mut pos = 0;

        // Field 1: max_capacity
        assert_eq!(payload[pos], TLV_MAX_CAPACITY);
        pos += 1;
        let len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
        pos += 2;
        assert_eq!(len, 4);
        let cap = u32::from_be_bytes(payload[pos..pos + 4].try_into().unwrap());
        assert_eq!(cap, 8);
        pos += len;

        // Field 2: api_endpoint
        assert_eq!(payload[pos], TLV_API_ENDPOINT);
        pos += 1;
        let len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
        pos += 2;
        assert_eq!(
            std::str::from_utf8(&payload[pos..pos + len]).unwrap(),
            "http://localhost:9300"
        );
        pos += len;

        // Field 3: supported_scorers
        assert_eq!(payload[pos], TLV_SUPPORTED_SCORERS);
        pos += 1;
        let len = u16::from_be_bytes([payload[pos], payload[pos + 1]]) as usize;
        pos += 2;
        assert_eq!(
            std::str::from_utf8(&payload[pos..pos + len]).unwrap(),
            "held_out_eval,private_oracle"
        );
        pos += len;

        assert_eq!(pos, payload.len(), "payload fully consumed");
    }

    #[test]
    fn empty_optional_fields_emit_only_capacity() {
        let payload = competitions_registration_payload(3, "", "");
        // Only max_capacity: 1 (type) + 2 (len) + 4 (u32) = 7 bytes.
        assert_eq!(payload.len(), 7);
        assert_eq!(payload[0], TLV_MAX_CAPACITY);
        let cap = u32::from_be_bytes(payload[3..7].try_into().unwrap());
        assert_eq!(cap, 3);
    }
}
