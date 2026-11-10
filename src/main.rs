// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2026, JieXu
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
mod app;
mod appearance;
mod background;
mod color;
mod controller;
mod display;
mod export;
mod frame_rate;
mod geometry;
mod gui;
mod icc;
mod model;
mod native;
mod picker;
mod sdl;
mod toolbar;
mod touch;
fn main() {
    if let Err(e) = start() {
        eprintln!("OverInk: {e}");
        std::process::exit(1)
    }
}
fn start() -> Result<(), String> {
    let mut options = app::Options {
        board: false,
        x11: false,
        sdr: false,
        display: 0,
        smoke: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--help" | "-h" => {
                println!(
                    "OverInk 0.1.0 — Rust 2024 / egui / SDL3\n\n  --whiteboard   Start with an opaque white background\n  --x11         Use X11 / XWayland instead of native Wayland\n  --sdr         Disable extended-linear-sRGB output\n  --display N   Start on display N (1-based)\n  --smoke-test  Render synthetic pen and touch events with SDL dummy video\n\nKeys (when an OverInk window has focus):\n  Space: click through / draw   H: hide / show\n  Esc: cancel stroke / click through   B: pencil   P/M/L/A/R/O/E/K: other tools\n  1–4: preset colors   Ctrl/Cmd+Z: undo   Ctrl/Cmd+Shift+Z: redo\n  Ctrl/Cmd+S: save   Ctrl/Cmd+O: open   Ctrl/Cmd+Shift+S: SVG\n\nDrag the control window to move a single canvas. Settings can assign a canvas to each display.\nDisconnected displays lose their page and undo history.\nFrame-rate choices: Auto, 120, 90, 60, 30 FPS. Auto follows the display.\nBackground choices: live desktop or frozen snapshot.\nThe separate control window remains usable in click-through mode.\nWhen session recovery is enabled, ink autosaves every 10 seconds and on normal exit.\nTouchscreens: draw with multiple fingers. Native pens take priority over finger drawing."
                );
                return Ok(());
            }
            "--whiteboard" => options.board = true,
            "--x11" => options.x11 = true,
            "--sdr" => options.sdr = true,
            "--smoke-test" => options.smoke = true,
            "--display" => {
                options.display = args
                    .next()
                    .ok_or("Missing display number")?
                    .parse::<usize>()
                    .map_err(|_| "Invalid display number")?
                    .checked_sub(1)
                    .ok_or("Display numbers start at 1")?
            }
            _ => return Err(format!("Unknown option {a}. Use --help")),
        }
    }
    app::run(options)
}
