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

    #[error(
        "this audio file has {0} channels — tebis only handles mono voice recordings. \
         Record a voice note (hold the mic button) instead of sending a music file."
    )]
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

    // A valid OpusHead is at least 19 bytes: 8-byte magic + 11 bytes of
    // fixed fields (version, channels, preskip, input_sample_rate,
    // output_gain, channel_mapping_family). Checking `< 19` catches
    // truncated / maliciously short headers up front; libopus would fail
    // later anyway but with a less useful error.
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
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

/// Encode mono PCM `f32` samples to an OGG/Opus byte blob suitable
/// for `POST /sendVoice`. 20 ms frames at `VoIP` complexity.
///
/// `sample_rate` must be one of Opus's native rates: 8000, 12000,
/// 16000, 24000, or 48000 Hz. Kokoro emits 24 kHz; the macOS `say`
/// backend emits 16 kHz. Encoding at the source rate avoids our own
/// (previously aliasing-prone) downsample step — Opus handles the
/// internal conversion losslessly.
///
/// Telegram's voice-message bubble renders OGG/Opus at any native
/// Opus rate; higher rates / other codecs land as a file attachment.
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
    // Per RFC 7845 §5.1, OpusHead preskip should be the encoder's
    // lookahead so decoders trim the warmup pop at the start of the
    // first frame. libopus reports this via opus_encoder_ctl;
    // `get_lookahead` is the opus crate's safe wrapper. Fallback to 0
    // if the call fails (no ambient audio issue — just a small click).
    let preskip: u16 = encoder
        .get_lookahead()
        .ok()
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(0);

    // Pad the input to a whole number of frames. Telegram tolerates the
    // ~20 ms of trailing silence, and Opus needs fixed-size input.
    let total_samples = pcm.len().div_ceil(frame_samples) * frame_samples;

    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(total_samples / frame_samples);
    let mut buf = vec![0u8; 4000]; // Opus packets typically < 500 B, but headroom is cheap.
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

    // Mux into an OGG container. Serial number is arbitrary (Telegram
    // ignores it but the OGG spec requires a unique stream-serial in
    // the page header); low 32 bits of the current micros work.
    // Casting after the `& 0xFFFF_FFFF` mask is infallible — we use
    // `as u32` rather than `try_from` so the dead `unwrap_or` fallback
    // goes away.
    #[allow(clippy::cast_possible_truncation, reason = "masked to 32 bits explicitly")]
    let serial: u32 = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() & 0xFFFF_FFFF)) as u32;

    let mut out: Vec<u8> = Vec::with_capacity(total_samples * 2 / 10);
    {
        let mut writer = ogg::writing::PacketWriter::new(std::io::Cursor::new(&mut out));

        // OpusHead (19 bytes): magic + version(1) + channels(1) +
        // preskip(0) + input_rate(16000) + output_gain(0) +
        // channel_mapping_family(0).
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(1); // channel count: mono
        head.extend_from_slice(&preskip.to_le_bytes());
        head.extend_from_slice(&sample_rate.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        head.push(0);
        writer
            .write_packet(head, serial, ogg::PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| CodecError::Encode(format!("ogg OpusHead: {e}")))?;

        // OpusTags: magic + empty vendor + empty user-comments.
        let mut tags = Vec::with_capacity(16);
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&0u32.to_le_bytes());
        tags.extend_from_slice(&0u32.to_le_bytes());
        writer
            .write_packet(tags, serial, ogg::PacketWriteEndInfo::EndPage, 0)
            .map_err(|e| CodecError::Encode(format!("ogg OpusTags: {e}")))?;

        // Audio packets. OGG Opus granule positions are counted at
        // 48 kHz regardless of encoded rate. For 16 kHz input: × 3 =
        // 960 per 20 ms frame. For 24 kHz: × 2 = 960. For 48 kHz: ×1
        // = 960. All 20 ms frames resolve to 960 at 48 kHz. (The prior
        // formula was off by a factor of 1000; fixed here.)
        let granule_per_frame = (frame_samples as u64) * 48_000 / u64::from(sample_rate);
        let mut granule: u64 = 0;
        // Use Peekable to identify the final packet without `len() - 1`
        // arithmetic (which would panic on empty `packets`, even though
        // the `pcm.is_empty()` check above already guarantees non-empty).
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

/// 20 ms frame size. Opus natively supports 2.5/5/10/20/40/60 ms;
/// 20 ms is Telegram voice's de-facto frame size. The actual sample
/// count per frame depends on the encoder's sample rate and is
/// computed inline in `encode_pcm_to_opus`.
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

    /// Round-trip exercise using both public codec fns: encode 1 s of
    /// silence, decode it back, assert sample count + amplitude.
    /// Replaces the old inline-encoder test now that `encode_pcm_to_opus`
    /// is a real public API.
    #[test]
    fn encode_decode_round_trip_silence() {
        let silence = vec![0.0_f32; OUTPUT_RATE as usize]; // 1 s
        let oga_bytes = encode_pcm_to_opus(&silence, OUTPUT_RATE).expect("encode");
        assert!(!oga_bytes.is_empty());
        // OGG pages always start with "OggS".
        assert_eq!(&oga_bytes[..4], b"OggS");

        let pcm = decode_opus_to_pcm16k(&oga_bytes).expect("decode");
        // Opus has a small internal preskip + latency; allow tolerance.
        assert!(
            pcm.len() > 14_000 && pcm.len() < 18_000,
            "unexpected sample count: {}",
            pcm.len()
        );
        let peak = pcm.iter().copied().map(f32::abs).fold(0.0_f32, f32::max);
        assert!(peak < 0.05, "silence decoded with peak {peak}");
    }

    #[test]
    fn encode_empty_input_errors() {
        let err = encode_pcm_to_opus(&[], OUTPUT_RATE).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
