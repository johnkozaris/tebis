//! Drive `platform::multiplexer::Mux` against the real psmux (Windows)
//! or tmux (Unix) binary end-to-end. Exercises the invariant-3 atomic
//! `-l text` → Enter sequence, invariant-13 target binding, invariant-15
//! NotFound recovery shape.
//!
//! Run with: `cargo run --release --example mux-smoke`
//! Requires: psmux on PATH (Windows) or tmux on PATH (Unix).

use tebis::platform::multiplexer::{Mux, MuxError};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let session = "mux_smoke_tebis";
    let mux = Mux::new(Vec::new(), 4000); // permissive

    // Clean slate.
    let _ = mux.kill_session(session).await;

    println!("[1/6] validate_session({session}) — invariant 2 regex");
    mux.validate_session(session)?;
    println!("     ok");

    println!("[2/6] new_session -d");
    mux.new_session(session, None, None).await?;
    println!("     ok");

    println!("[3/6] has_session — invariant 13 target binding");
    let present = mux.has_session(session).await?;
    assert!(present, "session should exist after new_session");
    println!("     ok: has_session = true");

    println!("[4/6] send_keys — invariant 3 atomic text + Enter");
    // Give the shell a moment to boot (pwsh startup on Windows).
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let beacon = "BEACON_a1b2c3d4e5";
    mux.send_keys(session, &format!("echo {beacon}")).await?;
    // Let the shell echo the line back through its rendering loop.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    println!("[5/6] capture_pane — verify beacon appears");
    let captured = mux.capture_pane(session, 30).await?;
    if captured.contains(beacon) {
        println!("     ok: '{beacon}' echoed back");
    } else {
        println!("     FAIL: beacon not in captured output");
        println!("     --- pane snapshot ---");
        println!("{captured}");
        println!("     --- end snapshot ---");
        anyhow::bail!("beacon not echoed — invariant 3 Enter did not reach the shell");
    }

    println!("[6/6] NotFound on nonexistent — invariant 15 classification");
    match mux.send_keys("nosuch_session_xyz", "noop").await {
        Err(MuxError::NotFound(_)) => println!("     ok: classified as NotFound"),
        Err(e) => {
            println!("     FAIL: expected NotFound, got {e}");
            anyhow::bail!("classify_status does not recognize this multiplexer's NotFound phrasing");
        }
        Ok(()) => anyhow::bail!("send-keys to missing session unexpectedly succeeded"),
    }

    mux.kill_session(session).await?;
    println!();
    println!("all six checks passed — Mux layer works against this multiplexer");
    Ok(())
}
