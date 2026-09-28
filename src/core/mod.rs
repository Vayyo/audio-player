//! Headless audio engine: decoding, format/rate conversion and playback state.
//!
//! The engine is GUI-agnostic and free of any audio output so that it can be
//! exercised entirely by `cargo test` without a sound card.

pub mod convert;
pub mod decode;
pub mod player;
pub mod queue;
