//! YouTube Music data access (InnerTube client + response parsing).

pub mod client;
pub mod domain;
pub mod extract;

pub use client::{Client, SearchFilter};
pub use domain::{ItemKind, Lyrics, Page, SearchItem, Track};
