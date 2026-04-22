//! OGG/Opus ⇄ PCM for Telegram voice via [`ogg`] + [`opus`].

use std::io::Cursor;

use bytes::Bytes;
use ogg::PacketReader;
use opus::{Channels, Decoder as OpusDecoder};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus decode failed: {0}")]
    Decode(String),

    #[error("opus encode failed: {0}")]
    Encode(String),

    #[error("input is not OGG/Opus (missing OpusHead magic in first packet)")]
    NotOpus,

    #[error(
        "this audio file has {0} channel(s) (tebis only handles mono voice recordings). \
         Record a voice note (hold the mic button) instead of sending a music file."
    )]
    UnsupportedChannels(u8),

    #[error("ogg read error: {0}")]
    OggRead(String),

    #[error(
        "decoded audio exceeds {max_samples} samples (16 kHz ≈ {max_sec}s) — \
         input may be a bitrate-stuffed adversarial blob"
    )]
    DecodedTooLarge { max_samples: usize, max_sec: u32 },
}

/// Headroom over Opus's 120 ms × 16 kHz = 1920 per-packet limit.
const MAX_SAMPLES_PER_PACKET: usize = 5760;

const OUTPUT_RATE: u32 = 16_000;

/// OGG/Opus → 16 kHz mono `f32` in `[-1.0, 1.0]`. `max_samples` caps
/// output — byte-input cap alone isn't enough (Opus compresses hard).
/// Rejects multi-channel; downmixing silently would be a footgun.
pub fn decode_opus_to_pcm16k(
    oga_bytes: &[u8],
    max_samples: usize,
) -> Result<Vec<f32>, CodecError> {
    if oga_bytes.is_empty() {
        return Err(CodecError::Decode("empty input".to_string()));
    }

    let mut reader = PacketReader::new(Cursor::new(oga_bytes));

    // OpusHead: magic at 0, channel count at 9. libopus handles preskip/rate/mapping.
    let head = reader
        .read_packet()
        .map_err(|e| CodecError::OggRead(e.to_string()))?
        .ok_or_else(|| CodecError::Decode("no packets in OGG stream".to_string()))?;

    // OpusHead ≥ 19 bytes (magic + fixed fields); shorter = truncated/malformed.
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
        return Err(CodecError::NotOpus);
    }
    let channel_count = head.data[9];
    if channel_count != 1 {
        return Err(CodecError::UnsupportedChannels(channel_count));
    }

    let _tags = reader
        .read_packet()
        .map_err(|e| CodecError::OggRead(e.to_string()))?
        .ok_or_else(|| CodecError::Decode("OpusTags packet missing".to_string()))?;

    let mut decoder = OpusDecoder::new(OUTPUT_RATE, Channels::Mono)
        .map_err(|e| CodecError::Decode(format!("decoder init: {e}")))?;

    let mut out: Vec<f32> = Vec::new();
    let mut buf = vec![0.0_f32; MAX_SAMPLES_PER_PACKET];

    loop {
        let packet = reader
            .read_packet()
            .map_err(|e| CodecError::OggRead(e.to_string()))?;
        let Some(packet) = packet else {
            break;
        };
        if packet.data.is_empty() {
            continue;
        }
        let n = decoder
            .decode_float(&packet.data, &mut buf, false)
            .map_err(|e| CodecError::Decode(format!("decode_float: {e}")))?;
        if out.len().saturating_add(n) > max_samples {
            return Err(CodecError::DecodedTooLarge {
                max_samples,
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "max_samples is a budget; the error message is informational"
                )]
                max_sec: (max_samples / 16_000).min(u32::MAX as usize) as u32,
            });
        }
        out.extend_from_slice(&buf[..n]);
    }

    if out.is_empty() {
        return Err(CodecError::Decode(
            "no audio packets decoded — stream may be malformed".to_string(),
        ));
    }

    Ok(out)
}

/// Mono `f32` PCM → OGG/Opus for `sendVoice`. 20 ms frames, VoIP.
/// `sample_rate` ∈ {8k, 12k, 16k, 24k, 48k} Hz.
pub fn encode_pcm_to_opus(pcm: &[f32], sample_rate: u32) -> Result<Bytes, CodecError> {
    use opus::{Application, Encoder as OpusEncoder};

    if pcm.is_empty() {
        return Err(CodecError::Encode("empty PCM input".to_string()));
    }
    if !matches!(sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
        return Err(CodecError::Encode(format!(
            "Opus requires 8/12/16/24/48 kHz; got {sample_rate} Hz"
        )));
    }
    let frame_samples: usize = (sample_rate as usize) * FRAME_MS / 1000;

    let mut encoder = OpusEncoder::new(sample_rate, Channels::Mono, Application::Voip)
        .map_err(|e| CodecError::Encode(format!("encoder init: {e}")))?;
    // Per RFC 7845 §5.1 — preskip = encoder lookahead trims warmup pop.
    let preskip: u16 = encoder
        .get_lookahead()
        .ok()
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(0);

    // Pad to whole frames; Opus needs fixed-size input.
    let total_samples = pcm.len().div_ceil(frame_samples) * frame_samples;

    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(total_samples / frame_samples);
    let mut buf = vec![0u8; 4000];
    let mut frame_buf = vec![0.0_f32; frame_samples];
    let mut offset = 0;
    while offset < total_samples {
        let end = (offset + frame_samples).min(pcm.len());
        let have = end - offset;
        frame_buf[..have].copy_from_slice(&pcm[offset..end]);
        if have < frame_samples {
            for sample in &mut frame_buf[have..] {
                *sample = 0.0;
            }
        }
        let n = encoder
            .encode_float(&frame_buf, &mut buf)
            .map_err(|e| CodecError::Encode(format!("encode_float: {e}")))?;
        packets.push(buf[..n].to_vec());
        offset += frame_samples;
    }

    // OGG serial is arbitrary but required to be unique — use clock micros.
    #[allow(clippy::cast_possible_truncation, reason = "masked to 32 bits explicitly")]
    let serial: u32 = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() & 0xFFFF_FFFF)) as u32;

    let mut out: Vec<u8> = Vec::with_capacity(total_samples * 2 / 10);
    {
        let mut writer = ogg::writing::PacketWriter::new(std::io::Cursor::new(&mut out));

        // OpusHead 19B: magic + ver + channels + preskip + rate + gain + mapping_family.
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1);
        head.push(1);
        head.extend_from_slice(&preskip.to_le_bytes());
        head.extend_from_slice(&sample_rate.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        writer
            .write_packet(head, serial, ogg::PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| CodecError::Encode(format!("ogg OpusHead: {e}")))?;

        let mut tags = Vec::with_capacity(16);
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&0u32.to_le_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        writer
            .write_packet(tags, serial, ogg::PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| CodecError::Encode(format!("ogg OpusTags: {e}")))?;

        // OGG Opus granule = 48 kHz ticks regardless of encode rate.
        let granule_per_frame = (frame_samples as u64) * 48_000 / u64::from(sample_rate);
        let mut granule: u64 = 0;
        let mut iter = packets.into_iter().peekable();
        while let Some(pkt) = iter.next() {
            granule += granule_per_frame;
            let end_kind = if iter.peek().is_none() {
                ogg::PacketWriteEndInfo::EndStream
            } else {
                ogg::PacketWriteEndInfo::NormalPacket
            };
            writer
                .write_packet(pkt, serial, end_kind, granule)
                .map_err(|e| CodecError::Encode(format!("ogg audio packet: {e}")))?;
        }
    }

    Ok(Bytes::from(out))
}

/// Telegram voice's de-facto frame size.
const FRAME_MS: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_variants_render() {
        assert!(
            CodecError::Decode("x".into())
                .to_string()
                .contains("opus decode")
        );
        assert!(
            CodecError::Encode("x".into())
                .to_string()
                .contains("opus encode")
        );
        assert!(
            CodecError::UnsupportedChannels(2)
                .to_string()
                .contains('2')
        );
        assert!(CodecError::NotOpus.to_string().contains("OpusHead"));
    }

    const TEST_SAMPLE_BUDGET: usize = 600 * 16_000;

    #[test]
    fn decode_empty_input_errors() {
        let err = decode_opus_to_pcm16k(&[], TEST_SAMPLE_BUDGET).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn decode_rejects_non_opus() {
        let garbage = vec![0xffu8; 16];
        let err = decode_opus_to_pcm16k(&garbage, TEST_SAMPLE_BUDGET).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("ogg") || msg.contains("OpusHead") || msg.contains("no packets"),
            "unexpected error message: {msg}",
        );
    }

    #[test]
    fn encode_decode_round_trip_silence() {
        let silence = vec![0.0_f32; OUTPUT_RATE as usize];
        let oga_bytes = encode_pcm_to_opus(&silence, OUTPUT_RATE).expect("encode");
        assert!(!oga_bytes.is_empty());
        assert_eq!(&oga_bytes[..4], b"OggS");

        let pcm = decode_opus_to_pcm16k(&oga_bytes, TEST_SAMPLE_BUDGET).expect("decode");
        // Opus preskip + latency — allow tolerance.
        assert!(
            pcm.len() > 14_000 && pcm.len() < 18_000,
            "unexpected sample count: {}",
            pcm.len()
        );
        let peak = pcm.iter().copied().map(f32::abs).fold(0.0_f32, f32::max);
        assert!(peak < 0.05, "silence decoded with peak {peak}");
    }

    #[test]
    fn decode_rejects_output_over_budget() {
        let two_sec = vec![0.0_f32; (OUTPUT_RATE as usize) * 2];
        let oga_bytes = encode_pcm_to_opus(&two_sec, OUTPUT_RATE).expect("encode");
        let budget = OUTPUT_RATE as usize;
        let err = decode_opus_to_pcm16k(&oga_bytes, budget).unwrap_err();
        assert!(
            matches!(err, CodecError::DecodedTooLarge { .. }),
            "expected DecodedTooLarge, got: {err:?}"
        );
    }

    #[test]
    fn encode_empty_input_errors() {
        let err = encode_pcm_to_opus(&[], OUTPUT_RATE).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
