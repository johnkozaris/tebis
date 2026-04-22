//! Download progress reporter. On a TTY → `indicatif` bar; otherwise
//! → throttled `tracing::info` (every 8 MiB) so systemd logs / launchd
//! logs don't get flooded with hundreds of progress lines per download.
//!
//! The bar goes to stderr so stdout remains clean for JSON callers.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Adapter between `fetch::FetchClient::download_verified`'s progress
/// callback (`FnMut(u64, Option<u64>)`) and either an `indicatif`
/// bar or a rate-limited `tracing::info`. Construct once per download,
/// pass its `update` closure to the fetch client, then call `finish`
/// when the fetch returns.
pub(super) enum Reporter {
    Bar(ProgressBar),
    /// Throttled log. `last_logged` is the last value we emitted at;
    /// we fire a new line every `LOG_EVERY` bytes.
    Log { last_logged: u64 },
}

impl Reporter {
    /// `label` is a short prefix shown left of the bar (`"Whisper small.en"`).
    /// `total_hint` is the expected byte count from the manifest; bars
    /// degrade to a spinner when unknown.
    pub(super) fn new(label: &str, total_hint: Option<u64>) -> Self {
        if console::Term::stderr().is_term() {
            let pb = match total_hint {
                Some(total) if total > 0 => ProgressBar::with_draw_target(
                    Some(total),
                    ProgressDrawTarget::stderr(),
                ),
                _ => ProgressBar::new_spinner().with_message(label.to_string()),
            };
            // Redraw at most 5× per second so fast LANs don't burn CPU on ANSI.
            pb.enable_steady_tick(Duration::from_millis(200));
            if total_hint.is_some_and(|t| t > 0) {
                let style = ProgressStyle::with_template(
                    "  {prefix:.bold.cyan} [{bar:30.cyan/blue}] {bytes:>10}/{total_bytes:<10} {binary_bytes_per_sec:>12}  ETA {eta:>4}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("█▉▊▋▌▍▎▏ ");
                pb.set_style(style);
                pb.set_prefix(label.to_string());
            }
            Self::Bar(pb)
        } else {
            Self::Log { last_logged: 0 }
        }
    }

    /// Consume progress. Wire this into `download_verified`'s callback
    /// via `|b, t| reporter.update(b, t)`.
    pub(super) fn update(&mut self, bytes: u64, total: Option<u64>) {
        match self {
            Self::Bar(pb) => {
                if let Some(t) = total
                    && t > 0
                    && pb.length() != Some(t)
                {
                    pb.set_length(t);
                }
                pb.set_position(bytes);
            }
            Self::Log { last_logged } => {
                const LOG_EVERY: u64 = 8 * 1024 * 1024;
                if bytes.saturating_sub(*last_logged) >= LOG_EVERY {
                    *last_logged = bytes;
                    if let Some(t) = total {
                        tracing::info!(
                            "  …downloaded {} / {} MB",
                            bytes / (1024 * 1024),
                            t / (1024 * 1024),
                        );
                    } else {
                        tracing::info!("  …downloaded {} MB", bytes / (1024 * 1024));
                    }
                }
            }
        }
    }

    /// End the bar (or log a single "done" line in non-TTY mode). `msg`
    /// is the final text shown to the right of the completed bar.
    pub(super) fn finish(self, msg: &str) {
        match self {
            Self::Bar(pb) => {
                pb.finish_with_message(msg.to_string());
            }
            Self::Log { .. } => {
                tracing::info!("  {msg}");
            }
        }
    }
}
