//! Minimal LE i16 WAV decoder shared by TTS backends that shell out to
//! tools returning WAV (macOS `say`, Windows WinRT `SpeechSynthesizer`).
//!
//! Scope is deliberately narrow: we only need to decode the single
//! `data` chunk into `f32` PCM. We never emit WAV ourselves.

use super::TtsError;

/// Parse a RIFF/WAVE byte slice containing LE i16 mono PCM into `f32`.
/// No rate/channel enforcement here — the caller states the expected
/// rate when constructing `Synthesis`.
pub fn parse_le_i16_wav(bytes: &[u8]) -> Result<Vec<f32>, TtsError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(TtsError::Synthesis(
            "not a RIFF/WAVE file — TTS output unexpected".to_string(),
        ));
    }

    let mut cursor = 12;
    loop {
        if cursor + 8 > bytes.len() {
            return Err(TtsError::Synthesis("unterminated WAV chunks".to_string()));
        }
        let chunk_id = &bytes[cursor..cursor + 4];
        let chunk_size = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let body_start = cursor + 8;
        // Checked — release has overflow-checks on; u32::MAX chunk_size would panic.
        let Some(body_end) = body_start.checked_add(chunk_size) else {
            return Err(TtsError::Synthesis(
                "WAV chunk size overflow — file is malformed".to_string(),
            ));
        };
        if body_end > bytes.len() {
            return Err(TtsError::Synthesis(format!(
                "WAV chunk {:?} runs past end of file",
                std::str::from_utf8(chunk_id).unwrap_or("??")
            )));
        }
        if chunk_id == b"data" {
            if !chunk_size.is_multiple_of(2) {
                return Err(TtsError::Synthesis(
                    "WAV data chunk has odd byte count — not i16 PCM".to_string(),
                ));
            }
            let sample_count = chunk_size / 2;
            let mut out = Vec::with_capacity(sample_count);
            for i in 0..sample_count {
                let lo = bytes[body_start + 2 * i];
                let hi = bytes[body_start + 2 * i + 1];
                let sample = i16::from_le_bytes([lo, hi]);
                out.push(f32::from(sample) / 32768.0);
            }
            return Ok(out);
        }
        cursor = body_end;
        if !chunk_size.is_multiple_of(2) {
            cursor += 1;
        }
    }
}

/// Read the `fmt ` chunk's sample-rate field. Returns 0 if not found.
/// Used by WinRT where `SpeechSynthesizer` picks a rate we don't set.
pub fn sample_rate_from_wav(bytes: &[u8]) -> u32 {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return 0;
    }
    let mut cursor = 12;
    while cursor + 8 <= bytes.len() {
        let chunk_id = &bytes[cursor..cursor + 4];
        let chunk_size = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let body_start = cursor + 8;
        if chunk_id == b"fmt " && body_start + 12 <= bytes.len() {
            // WAVE fmt layout: fmt_tag(2) channels(2) sample_rate(4) ...
            return u32::from_le_bytes([
                bytes[body_start + 4],
                bytes[body_start + 5],
                bytes[body_start + 6],
                bytes[body_start + 7],
            ]);
        }
        let Some(next) = body_start.checked_add(chunk_size) else {
            return 0;
        };
        cursor = next + usize::from(!chunk_size.is_multiple_of(2));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_minimal_wav() -> Vec<u8> {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16_000_u32.to_le_bytes());
        wav.extend_from_slice(&32_000_u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&6u32.to_le_bytes());
        wav.extend_from_slice(&0i16.to_le_bytes());
        wav.extend_from_slice(&1i16.to_le_bytes());
        wav.extend_from_slice(&(-1i16).to_le_bytes());
        wav
    }

    #[test]
    fn parse_minimal_wav() {
        let wav = write_minimal_wav();
        let pcm = parse_le_i16_wav(&wav).expect("parse");
        assert_eq!(pcm.len(), 3);
        assert!((pcm[0] - 0.0).abs() < 1e-9);
        assert!(pcm[1] > 0.0 && pcm[1] < 1e-3);
        assert!(pcm[2] < 0.0 && pcm[2] > -1e-3);
    }

    #[test]
    fn parse_wav_rejects_non_riff() {
        let garbage = vec![0u8; 64];
        assert!(parse_le_i16_wav(&garbage).is_err());
    }

    #[test]
    fn parse_wav_rejects_odd_data_size() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&3u32.to_le_bytes());
        wav.extend_from_slice(&[0, 0, 0]);
        let err = parse_le_i16_wav(&wav).unwrap_err();
        assert!(err.to_string().contains("odd byte count"));
    }

    #[test]
    fn sample_rate_reads_fmt_chunk() {
        let wav = write_minimal_wav();
        assert_eq!(sample_rate_from_wav(&wav), 16_000);
    }
}
