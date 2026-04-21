//! OGG/Opus ⇄ PCM codec for Telegram voice messages.
//!
//! Tebis inbound path: Telegram voice notes arrive as OGG containers
//! holding Opus-encoded 16 kHz mono audio. `whisper-rs` wants `Vec<f32>`
//! at 16 kHz mono in `[-1.0, 1.0]`. This module bridges that.
//!
//! Implementation uses:
//! - [`ogg`] (pure-Rust OGG container demux) to split the stream into
//!   packets.
//! - [`opus`] (safe bindings to `libopus`) to decode each audio packet
//!   into PCM `f32`. The decoder is configured at 16 kHz so Opus's
//!   own resampler handles any 48→16 kHz conversion internally — we
//!   never touch resampling ourselves.
//!
//! The outbound (TTS) encode path lives as a stub — Phase 4 work.

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

    #[error("unsupported channel configuration: {0} (only mono is supported)")]
    UnsupportedChannels(u8),

    #[error("ogg read error: {0}")]
    OggRead(String),
}

/// Maximum PCM samples a single Opus packet can decode to, per channel.
/// Opus's hard limit is 120 ms; at 16 kHz that's 1920 samples. We use
/// double that (5760) as headroom — cheap slack against future Opus
/// quirks, and whisper-rs ignores unused trailing samples anyway.
const MAX_SAMPLES_PER_PACKET: usize = 5760;

/// Expected sample rate for whisper-rs input.
const OUTPUT_RATE: u32 = 16_000;

/// Decode an OGG/Opus byte blob (e.g. a Telegram `voice` download) into
/// 16 kHz mono PCM `f32` samples in `[-1.0, 1.0]`. The first two OGG
/// packets (`OpusHead` + `OpusTags`) are metadata and skipped.
///
/// Rejects multi-channel input outright — Telegram voice is always
/// mono, and silently downmixing stereo would be a footgun. If you hit
/// [`CodecError::UnsupportedChannels`] in practice, it means Telegram
/// shipped a music file via `sendAudio`, not a voice note; the bridge
/// should either re-mux to mono before calling this or reject the
/// attachment upstream.
pub fn decode_opus_to_pcm16k(oga_bytes: &[u8]) -> Result<Vec<f32>, CodecError> {
    if oga_bytes.is_empty() {
        return Err(CodecError::Decode("empty input".to_string()));
    }

    let mut reader = PacketReader::new(Cursor::new(oga_bytes));

    // First packet: OpusHead. Magic `b"OpusHead"` at offset 0, channel
    // count at offset 9. We parse just enough to reject stereo up-front
    // — the rest of the header (preskip, input_sample_rate, output_gain,
    // channel_mapping) doesn't matter because we ask libopus to emit
    // 16 kHz mono regardless of source rate.
    let head = reader
        .read_packet()
        .map_err(|e| CodecError::OggRead(e.to_string()))?
        .ok_or_else(|| CodecError::Decode("no packets in OGG stream".to_string()))?;

    if head.data.len() < 10 || &head.data[..8] != b"OpusHead" {
        return Err(CodecError::NotOpus);
    }
    let channel_count = head.data[9];
    if channel_count != 1 {
        return Err(CodecError::UnsupportedChannels(channel_count));
    }

    // Second packet: OpusTags (metadata). Ignored.
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
        out.extend_from_slice(&buf[..n]);
    }

    if out.is_empty() {
        return Err(CodecError::Decode(
            "no audio packets decoded — stream may be malformed".to_string(),
        ));
    }

    Ok(out)
}

/// Encode 16 kHz mono PCM `f32` samples to an OGG/Opus byte blob suitable
/// for `POST /sendVoice`. **Phase 4 stub.**
pub fn encode_pcm_to_opus(_pcm: &[f32]) -> Result<Bytes, CodecError> {
    todo!("Phase 4: wire opus encode + ogg mux for /sendVoice")
}

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

    #[test]
    fn decode_empty_input_errors() {
        let err = decode_opus_to_pcm16k(&[]).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn decode_rejects_non_opus() {
        // 16 bytes of garbage — not OpusHead, not even a valid OGG page,
        // so the ogg reader should error out before we get to the magic
        // check. Either error is fine; we just need a clean error path.
        let garbage = vec![0xffu8; 16];
        let err = decode_opus_to_pcm16k(&garbage).unwrap_err();
        // Don't assert on the specific variant — ogg-crate's parser
        // may report it as an OggRead or the downstream packet reader
        // may surface it differently across versions.
        let msg = err.to_string();
        assert!(
            msg.contains("ogg") || msg.contains("OpusHead") || msg.contains("no packets"),
            "unexpected error message: {msg}",
        );
    }

    /// Synthesize a minimal OGG/Opus round-trip locally to exercise the
    /// decode path end-to-end without a fixture file. We use `opus` to
    /// encode 1 second of silence, then wrap the packets in a bare OGG
    /// container and decode it back.
    #[test]
    fn decode_round_trip_silence() {
        use opus::Application;
        use std::io::Write as _;

        // 1 second of silence at 16 kHz mono = 16000 samples.
        const SAMPLE_RATE: u32 = 16_000;
        const FRAME_MS: usize = 20;
        const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize) * FRAME_MS / 1000;
        const TOTAL_FRAMES: usize = 1000 / FRAME_MS;

        // Encode 50 × 20ms frames of silence.
        let mut encoder = opus::Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)
            .expect("encoder init");
        let silence = vec![0.0_f32; FRAME_SAMPLES];
        let mut packets: Vec<Vec<u8>> = Vec::new();
        for _ in 0..TOTAL_FRAMES {
            let mut pkt = vec![0u8; 4000];
            let n = encoder
                .encode_float(&silence, &mut pkt)
                .expect("encode packet");
            pkt.truncate(n);
            packets.push(pkt);
        }

        // Mux into an OGG container using the ogg crate's PacketWriter.
        let mut oga_bytes: Vec<u8> = Vec::new();
        {
            let mut writer =
                ogg::writing::PacketWriter::new(std::io::Cursor::new(&mut oga_bytes));
            let serial = 0xA1B2_C3D4;

            // OpusHead: 8-byte magic + version(1) + channels(1) + preskip(0)
            // + input_sample_rate(16000) + output_gain(0) + mapping_family(0)
            let mut head = Vec::with_capacity(19);
            head.extend_from_slice(b"OpusHead");
            head.push(1); // version
            head.push(1); // mono
            head.extend_from_slice(&0u16.to_le_bytes()); // preskip
            head.extend_from_slice(&SAMPLE_RATE.to_le_bytes()); // input rate
            head.extend_from_slice(&0i16.to_le_bytes()); // output gain
            head.push(0); // channel mapping family
            writer
                .write_packet(head, serial, ogg::PacketWriteEndInfo::EndPage, 0)
                .unwrap();

            // OpusTags: magic + empty vendor + empty user-comment list.
            let mut tags = Vec::with_capacity(16);
            tags.extend_from_slice(b"OpusTags");
            tags.extend_from_slice(&0u32.to_le_bytes()); // vendor length = 0
            tags.extend_from_slice(&0u32.to_le_bytes()); // user-comment count = 0
            writer
                .write_packet(tags, serial, ogg::PacketWriteEndInfo::EndPage, 0)
                .unwrap();

            let last_idx = packets.len() - 1;
            let mut granule: u64 = 0;
            for (i, pkt) in packets.into_iter().enumerate() {
                granule += FRAME_SAMPLES as u64 * 48 / 16; // OGG Opus granules use 48kHz
                let end = if i == last_idx {
                    ogg::PacketWriteEndInfo::EndStream
                } else {
                    ogg::PacketWriteEndInfo::NormalPacket
                };
                writer.write_packet(pkt, serial, end, granule).unwrap();
            }
            writer.inner_mut().flush().unwrap();
        }

        let pcm = decode_opus_to_pcm16k(&oga_bytes).expect("decode");
        // 1 second of silence at 16 kHz ≈ 16000 samples. Opus has a
        // preskip + small internal latency so we allow some tolerance.
        assert!(
            pcm.len() > 14_000 && pcm.len() < 18_000,
            "unexpected sample count: {}",
            pcm.len()
        );
        // All samples should be near zero (silence).
        let peak = pcm.iter().copied().map(f32::abs).fold(0.0_f32, f32::max);
        assert!(peak < 0.05, "silence decoded with peak {peak}");
    }
}
