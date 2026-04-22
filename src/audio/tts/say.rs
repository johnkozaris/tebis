//! macOS `say` TTS backend. Runs
//! `say --file-format=WAVE --data-format=LEI16@16000 -v <voice> -o <tmp>`,
//! reads the WAV back, parses LE i16 → f32, unlinks. 30 s timeout.

use std::fs;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::time::timeout;

use super::{Synthesis, Tts, TtsError};

const SAY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct SayTts;

impl SayTts {
    pub const fn new() -> Self {
        Self
    }

    /// Invokes `say -?` — non-zero exit is expected; we only care it's runnable.
    pub async fn probe() -> Result<(), TtsError> {
        Command::new("say")
            .arg("-?")
            .output()
            .await
            .map_err(|e| TtsError::Init(format!("`say` not runnable: {e}")))?;
        Ok(())
    }
}

impl Tts for SayTts {
    async fn synthesize(&self, text: &str, voice: &str) -> Result<Synthesis, TtsError> {
        if text.trim().is_empty() {
            return Err(TtsError::Synthesis("empty text".to_string()));
        }

        let start = Instant::now();
        let tmp = std::env::temp_dir().join(format!(
            "tebis-say-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        ));

        // O_CREAT|O_EXCL mode 0600 defeats a symlink race on the tmp path.
        {
            use std::os::unix::fs::OpenOptionsExt;
            if let Err(e) = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
            {
                return Err(TtsError::Synthesis(format!(
                    "tmp-file pre-create {}: {e}",
                    tmp.display()
                )));
            }
        }

        let mut cmd = Command::new("say");
        cmd.arg("--file-format=WAVE")
            .arg("--data-format=LEI16@16000")
            .arg("-o")
            .arg(&tmp);
        if !voice.is_empty() {
            cmd.arg("-v").arg(voice);
        }
        cmd.arg(text);

        let run = cmd.output();
        let run_result = timeout(SAY_TIMEOUT, run).await;
        let output = match run_result {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                let _ = fs::remove_file(&tmp);
                return Err(TtsError::Synthesis(format!("spawn failed: {e}")));
            }
            Err(_) => {
                let _ = fs::remove_file(&tmp);
                return Err(TtsError::Synthesis(format!(
                    "say timed out after {SAY_TIMEOUT:?}"
                )));
            }
        };
        if !output.status.success() {
            let _ = fs::remove_file(&tmp);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(TtsError::Synthesis(format!(
                "say exited {:?}: {stderr}",
                output.status.code()
            )));
        }

        let wav_bytes = fs::read(&tmp)
            .map_err(|e| TtsError::Synthesis(format!("read {}: {e}", tmp.display())))?;
        let _ = fs::remove_file(&tmp);

        let pcm = parse_le_i16_wav(&wav_bytes)?;
        if pcm.is_empty() {
            return Err(TtsError::EmptyOutput);
        }

        Ok(Synthesis {
            pcm,
            duration_ms: u32::try_from(start.elapsed().as_millis()).unwrap_or(u32::MAX),
            sample_rate: 16_000,
        })
    }
}

/// Minimal WAV reader — assumes `say` produced LEI16 @ 16 kHz mono (what we asked for).
fn parse_le_i16_wav(bytes: &[u8]) -> Result<Vec<f32>, TtsError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(TtsError::Synthesis(
            "not a RIFF/WAVE file — `say` output unexpected".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_wav() {
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
}
