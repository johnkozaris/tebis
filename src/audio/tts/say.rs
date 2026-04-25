//! macOS `say` TTS backend. Runs
//! `say --file-format=WAVE --data-format=LEI16@16000 -v <voice> -o <tmp>`,
//! reads the WAV back, parses LE i16 → f32, unlinks. 30 s timeout.

use std::fs;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::time::timeout;

use super::{Synthesis, Tts, TtsError, wav};

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

        let pcm = wav::parse_le_i16_wav(&wav_bytes)?;
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

// WAV decoding lives in `wav` module so the Windows WinRT backend can share it.
