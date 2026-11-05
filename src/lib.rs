//! Kenkurs — a native, lightweight rewrite of [Kenku FM](https://github.com/owlbear-rodeo/kenku-fm).
//!
//! The crate is organised in layers, bottom-up:
//!
//! - [`audio`]: the real-time audio engine (decoding, resampling, mixing, fades, output devices).
//! - [`state`]: plain application data and pure logic (library, play queue, settings, persistence).
//! - [`app`]: the controller tying state and engine together — the port of Kenku FM's playback
//!   hooks and Redux slices — plus the threaded runtime that hosts it.
//! - `ui` (egui frontend): next milestone; it will only talk to [`app`].

#![forbid(unsafe_code)]

pub mod app;
pub mod audio;
pub mod state;
pub mod util;
