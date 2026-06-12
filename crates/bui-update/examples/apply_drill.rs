//! Drill for `apply_and_relaunch` without a GUI: swap a scratch "exe"
//! for a staged one and relaunch it. Exits 0 via the relaunch path on
//! success; the caller then checks the scratch dir:
//!
//!   exe        — the formerly-staged binary (swap happened)
//!   exe.old    — the original binary, moved aside
//!
//! Usage: cargo run -p bui-update --example apply_drill -- <exe> <staged>
//! Both paths must exist and be executable; <exe> is spawned after the
//! swap, so point it at something that exits on its own (e.g. a copy of
//! /usr/bin/true).

use bui_update::{StagedUpdate, Version, apply_and_relaunch};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(exe), Some(staged)) = (args.next(), args.next()) else {
        eprintln!("usage: apply_drill <exe> <staged>");
        std::process::exit(2);
    };
    let staged = StagedUpdate {
        version: Version::parse("9.9.9").unwrap(),
        staged_path: staged.into(),
        exe_path: exe.into(),
    };
    // Returns only on failure — success ends in process::exit(0).
    let err = apply_and_relaunch(&staged);
    eprintln!("apply failed: {err}");
    std::process::exit(1);
}
