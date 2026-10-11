//! Audio: resolving YouTube streams to files and playing them back.

pub mod engine;
pub mod resolver;
pub mod stream;

pub use engine::AudioEngine;
