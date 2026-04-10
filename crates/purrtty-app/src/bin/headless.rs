//! Headless terminal test harness.
//!
//! Spawns a real shell in a PTY, feeds its output through our VT
//! parser for a few seconds, drains and writes back DA/DSR responses,
//! then dumps the grid contents so we can inspect what the terminal
//! would show.
//!
//! Usage:
//!     cargo run --bin headless

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use purrtty_pty::PtySession;
use purrtty_term::Terminal;

fn main() {
    let rows = 24u16;
    let cols = 80u16;
    let terminal = Arc::new(Mutex::new(Terminal::new(rows as usize, cols as usize)));

    // Shared queue for terminal responses (DA, DSR, etc.)
    let responses: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let resp_for_reader = responses.clone();
    let term_for_reader = terminal.clone();

    let mut pty = PtySession::spawn(rows, cols, move |bytes| {
        let mut term = term_for_reader.lock().unwrap();
        term.advance(bytes);
        let resps = term.grid_mut().drain_responses();
        if !resps.is_empty() {
            resp_for_reader.lock().unwrap().extend(resps);
        }
    })
    .expect("failed to spawn pty");

    // Poll for responses and write them back to the PTY for 3 seconds.
    for i in 0..30 {
        thread::sleep(Duration::from_millis(100));
        let resps: Vec<Vec<u8>> = {
            let mut r = responses.lock().unwrap();
            std::mem::take(&mut *r)
        };
        for resp in &resps {
            let _ = pty.write(resp);
            eprintln!(
                "[{:>4}ms] wrote response: {:?}",
                (i + 1) * 100,
                String::from_utf8_lossy(resp)
            );
        }
    }

    // Dump the grid state.
    let term = terminal.lock().unwrap();
    let grid = term.grid();
    println!();
    println!("=== Grid dump ({}×{}) ===", grid.rows(), grid.cols());
    for row_idx in 0..grid.rows() {
        let mut line = String::new();
        for col_idx in 0..grid.cols() {
            let ch = grid.cell(row_idx, col_idx).ch;
            if ch == '\0' {
                // WIDE_CONT sentinel — skip
                continue;
            }
            line.push(ch);
        }
        let trimmed = line.trim_end();
        if !trimmed.is_empty() || row_idx == grid.cursor().row {
            println!(
                "{:>3}{} |{}|",
                row_idx,
                if row_idx == grid.cursor().row {
                    "*"
                } else {
                    " "
                },
                trimmed
            );
        }
    }
    println!();
    println!(
        "Cursor: row={}, col={}",
        grid.cursor().row,
        grid.cursor().col
    );
    println!("Cursor visible: {}", grid.cursor_visible());
    println!("CWD (OSC 7): {:?}", grid.cwd());
    println!("Scrollback rows: {}", grid.scrollback_len());
    println!("Alt screen: {}", grid.is_alt_screen());
    println!(
        "Pending responses: {}",
        responses.lock().unwrap().len()
    );
}
