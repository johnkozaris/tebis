//! Voice-embedding binary loader.
//!
//! Each HuggingFace Kokoro voice file (e.g. `af_sarah.bin`) is a raw
//! **little-endian `f32` byte blob**, shape `(N, 1, 256)` where `N`
//! is the max sequence length the voice was trained against (510 for
//! Kokoro v1.0). It is NOT a `.npy` file — no magic header, no shape
//! metadata, just 130 560 `f32`s packed end-to-end.
//!
//! (The `.npy`/`.npz`-style bundle called `voices-v1.0.bin` exists
//! upstream — that's what `kokoros` / `kokoroxide` load — but the
//! per-voice files under `onnx-community/Kokoro-82M-v1.0-ONNX/voices/`
//! are raw arrays. We use the per-voice files because they let us
//! download only the voices a user configures, not a 10 MB bundle.)
//!
//! The voice-indexing trick, confirmed in `kokoro-onnx/__init__.py`:
//! the style vector Kokoro wants at synth time depends on the token
//! count. Pick row `len(tokens)` (pre-boundary-pads) from the table
//! and pass that `(1, 256)` slice as the `style` input.
//!
//! ```python
//! style = voice[len(tokens)]   # shape (1, 256)
//! ```
//!
//! Without the row-by-token-count indexing you get a single static
//! style that doesn't match the utterance length — audibly wrong
//! prosody.

#![cfg(feature = "kokoro")]

use std::path::Path;

use ndarray::{Array2, Array3, Axis};

use super::super::TtsError;

/// Per-row inner dim. Fixed by the Kokoro v1.0 model signature.
const STYLE_DIM: usize = 256;
/// Bytes per row = 1 × 256 × sizeof(f32).
const BYTES_PER_ROW: usize = STYLE_DIM * 4;

/// Owned voice lookup table. Shape: `(max_seq_len, 1, 256)`.
///
/// `Array3<f32>` (not `Arc<...>`) because synth calls are rare enough
/// that ndarray's internal refcount isn't worth the ceremony; caller
/// clones on the `(1, 256)` row extraction, which is a 1 KB copy.
#[derive(Debug)]
pub struct Voice {
    table: Array3<f32>,
}

impl Voice {
    /// Load a per-voice raw-f32 `.bin` file from disk.
    ///
    /// Errors with [`TtsError::Init`] if:
    /// - file doesn't exist / unreadable
    /// - byte count isn't a multiple of 1024 (= 1 × 256 × 4 bytes)
    /// - the implied outer dim is zero
    pub fn load(path: &Path) -> Result<Self, TtsError> {
        let bytes = std::fs::read(path)
            .map_err(|e| TtsError::Init(format!("read voice `{}`: {e}", path.display())))?;

        if bytes.is_empty() {
            return Err(TtsError::Init(format!(
                "voice `{}` is empty",
                path.display()
            )));
        }
        if !bytes.len().is_multiple_of(BYTES_PER_ROW) {
            return Err(TtsError::Init(format!(
                "voice `{}` size {} bytes not divisible by {BYTES_PER_ROW} (1×256 f32 per row)",
                path.display(),
                bytes.len(),
            )));
        }

        let n_rows = bytes.len() / BYTES_PER_ROW;
        let n_floats = n_rows * STYLE_DIM;

        // LE f32 parse. Could be unsafe-transmuted on platforms where we
        // know alignment + endianness are already-native, but the safe
        // path is ~1 ms for 130k floats — negligible next to the ~500 ms
        // ort session init on the same call path.
        let mut floats = Vec::with_capacity(n_floats);
        for chunk in bytes.chunks_exact(4) {
            floats.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }

        let table = Array3::from_shape_vec((n_rows, 1, STYLE_DIM), floats).map_err(|e| {
            TtsError::Init(format!(
                "voice `{}` reshape to ({n_rows}, 1, {STYLE_DIM}): {e}",
                path.display(),
            ))
        })?;

        Ok(Self { table })
    }

    /// Max token count this voice embedding can service.
    #[must_use]
    pub fn max_tokens(&self) -> usize {
        self.table.dim().0
    }

    /// Extract the `(1, 256)` style slice for a sequence of `n_tokens`
    /// phonemes. Saturates to the last row when `n_tokens > max_tokens`
    /// so short-by-one boundary cases (off-by-one on pad counting)
    /// don't panic; the produced audio will be mildly less "shaped"
    /// but still intelligible. Empty inputs clamp to row 0.
    ///
    /// Returns an owned `Array2<f32>` because ort's input-building
    /// consumes the value — borrowing the view through the async
    /// synth call would require a lifetime we don't want to thread.
    #[must_use]
    pub fn style_for_token_count(&self, n_tokens: usize) -> Array2<f32> {
        let last = self.max_tokens().saturating_sub(1);
        let row = n_tokens.min(last);
        // index_axis gives an `(1, 256)` view; `to_owned` materializes
        // a fresh 1 KB buffer. Negligible next to 200-400 ms of inference.
        self.table.index_axis(Axis(0), row).to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a synthetic raw-f32 voice blob for testing. Row `i` is
    /// filled with `i * 0.001` so tests can verify row selection
    /// without computing hashes.
    fn synth_voice(n_rows: usize) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!(
            "tebis-voice-test-{}-{:?}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        ));
        let mut bytes = Vec::with_capacity(n_rows * BYTES_PER_ROW);
        for i in 0..n_rows {
            #[allow(
                clippy::cast_precision_loss,
                reason = "test data; values well under f32 precision limit"
            )]
            let v = (i as f32) * 0.001;
            for _ in 0..STYLE_DIM {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
        std::fs::write(&tmp, &bytes).expect("write test voice");
        tmp
    }

    #[test]
    fn load_valid_shape_succeeds() {
        let path = synth_voice(510);
        let v = Voice::load(&path).expect("load");
        assert_eq!(v.max_tokens(), 510);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_empty_file() {
        let tmp = std::env::temp_dir().join(format!(
            "tebis-voice-empty-{}.bin",
            std::process::id()
        ));
        std::fs::write(&tmp, b"").expect("write empty");
        let err = Voice::load(&tmp).expect_err("must reject empty");
        match err {
            TtsError::Init(msg) => assert!(msg.contains("empty")),
            other => panic!("unexpected: {other:?}"),
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn load_rejects_misaligned_size() {
        let tmp = std::env::temp_dir().join(format!(
            "tebis-voice-bad-{}.bin",
            std::process::id()
        ));
        // Off by one — not a multiple of 1024.
        std::fs::write(&tmp, vec![0_u8; 1025]).expect("write misaligned");
        let err = Voice::load(&tmp).expect_err("must reject misaligned");
        match err {
            TtsError::Init(msg) => assert!(
                msg.contains("not divisible"),
                "wrong error: {msg}"
            ),
            other => panic!("unexpected: {other:?}"),
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn style_for_token_count_picks_correct_row() {
        let path = synth_voice(10);
        let v = Voice::load(&path).expect("load");
        // Row 3 was filled with 0.003f32.
        let style = v.style_for_token_count(3);
        assert_eq!(style.dim(), (1, STYLE_DIM));
        assert!((style[[0, 0]] - 0.003).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn style_for_token_count_saturates_at_last_row() {
        let path = synth_voice(10);
        let v = Voice::load(&path).expect("load");
        // Out-of-bounds token count picks the last row (row 9 → 0.009).
        let style = v.style_for_token_count(999);
        assert!((style[[0, 0]] - 0.009).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn style_for_token_count_zero_uses_row_zero() {
        let path = synth_voice(10);
        let v = Voice::load(&path).expect("load");
        let style = v.style_for_token_count(0);
        assert!((style[[0, 0]] - 0.0).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_real_voice_size_sanity() {
        // The real af_sarah.bin is 522 240 bytes = 510 rows × 1024.
        let path = synth_voice(510);
        let v = Voice::load(&path).expect("load");
        assert_eq!(v.max_tokens(), 510);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 522_240);
        let _ = std::fs::remove_file(&path);
    }
}
