//! What is in the audio cache, and what to keep.
//!
//! The cache is files named after video ids, which on its own cannot say what
//! any of them *is*: no title, no artist, no artwork. This keeps a small record
//! beside each file, so a Downloads view can show something a person
//! recognises, and so "keep this" has somewhere to live.
//!
//! The filesystem stays the authority on what is cached. A file removed by the
//! eviction planner, or by hand, must not linger here as a row that plays
//! nothing, so the index is reconciled against the directory whenever it is
//! read rather than trusted.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::audio::resolver;
use crate::config;
use crate::ytm::Track;

/// One cached track, as stored.
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub track: Track,
    /// Exempt from eviction however old it gets. This is what "download" means,
    /// as opposed to "happened to be cached on the way past".
    #[serde(default)]
    pub keep: bool,
}

/// A cached track, with what the filesystem says about it right now.
pub struct CachedTrack {
    pub track: Track,
    pub keep: bool,
    pub size: u64,
    /// When the file was last written, which is when it was last played.
    pub modified: SystemTime,
}

/// Where the index lives. Not inside `audio/`, which is walked for `.m4a`
/// files by both the usage meter and the eviction planner.
fn index_path() -> PathBuf {
    config::cache_dir().join("downloads.jsonl")
}

/// The index of what is cached.
#[derive(Default)]
pub struct Downloads {
    path: PathBuf,
    /// Where cached audio lives. Held rather than looked up, so tests can point
    /// a whole index at a temporary directory.
    dir: PathBuf,
    entries: HashMap<String, Entry>,
}

impl Downloads {
    /// Read the index, dropping anything whose file has gone.
    pub fn load() -> Self {
        Self::open(index_path(), resolver::cache_dir())
    }

    fn open(path: PathBuf, dir: PathBuf) -> Self {
        let mut entries: HashMap<String, Entry> = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let Ok(entry) = serde_json::from_str::<Entry>(line) else {
                    continue;
                };
                // A record without a file is a row that would play nothing.
                if dir.join(format!("{}.m4a", entry.track.video_id)).exists() {
                    entries.insert(entry.track.video_id.clone(), entry);
                }
            }
        }
        // Cached files with no record still belong in the list: they were
        // fetched before this index existed, or by a path that does not write
        // to it. The video id is all there is to call them until the track is
        // played again and hands over its real metadata.
        if let Ok(files) = std::fs::read_dir(&dir) {
            for file in files.flatten() {
                let cached = file.path();
                if cached.extension().is_none_or(|ext| ext != "m4a") {
                    continue;
                }
                let Some(id) = cached.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                entries.entry(id.to_string()).or_insert_with(|| Entry {
                    track: Track {
                        video_id: id.to_string(),
                        title: id.to_string(),
                        ..Default::default()
                    },
                    keep: false,
                });
            }
        }
        Self { path, dir, entries }
    }

    fn file(&self, video_id: &str) -> PathBuf {
        self.dir.join(format!("{video_id}.m4a"))
    }

    /// Record that `track` is cached.
    pub fn record(&mut self, track: &Track) {
        let keep = self
            .entries
            .get(&track.video_id)
            .is_some_and(|entry| entry.keep);
        self.entries.insert(
            track.video_id.clone(),
            Entry {
                track: track.clone(),
                keep,
            },
        );
        self.save();
    }

    /// Drop a track's record, its file having gone.
    pub fn forget(&mut self, video_id: &str) {
        if self.entries.remove(video_id).is_some() {
            self.save();
        }
    }

    /// Mark a cached track as kept, or not. False if it is not cached at all.
    pub fn set_keep(&mut self, video_id: &str, keep: bool) -> bool {
        match self.entries.get_mut(video_id) {
            Some(entry) => {
                entry.keep = keep;
                self.save();
                true
            }
            None => false,
        }
    }

    /// Whether a track is marked as kept.
    pub fn is_kept(&self, video_id: &str) -> bool {
        self.entries.get(video_id).is_some_and(|entry| entry.keep)
    }

    /// The video ids that must not be evicted.
    pub fn keep_set(&self) -> HashSet<String> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.keep)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Everything cached, newest first.
    pub fn cached(&self) -> Vec<CachedTrack> {
        let mut out: Vec<CachedTrack> = self
            .entries
            .values()
            .filter_map(|entry| {
                let meta = std::fs::metadata(self.file(&entry.track.video_id)).ok()?;
                Some(CachedTrack {
                    track: entry.track.clone(),
                    keep: entry.keep,
                    size: meta.len(),
                    modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                })
            })
            .collect();
        out.sort_by_key(|item| std::cmp::Reverse(item.modified));
        out
    }

    /// Write the index back.
    ///
    /// Renamed into place: a crash mid-write leaves the previous index rather
    /// than a truncated one, which is the difference between losing the most
    /// recent play and losing every title in the cache.
    fn save(&self) {
        if let Some(parent) = self.path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return;
        }
        let mut text = String::new();
        for entry in self.entries.values() {
            if let Ok(line) = serde_json::to_string(entry) {
                text.push_str(&line);
                text.push('\n');
            }
        }
        let temp = self.path.with_extension("jsonl.tmp");
        if std::fs::write(&temp, text).is_ok() {
            let _ = std::fs::rename(&temp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, title: &str) -> Track {
        Track {
            video_id: id.to_string(),
            title: title.to_string(),
            ..Default::default()
        }
    }

    /// A fresh index in a temporary directory, with the given files present.
    fn fixture(name: &str, cached: &[&str]) -> (PathBuf, Downloads) {
        let dir = std::env::temp_dir().join(format!("refrain-downloads-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for id in cached {
            std::fs::write(dir.join(format!("{id}.m4a")), b"audio").unwrap();
        }
        let downloads = Downloads::open(dir.join("downloads.jsonl"), dir.clone());
        (dir, downloads)
    }

    #[test]
    fn records_and_reloads() {
        let (dir, mut downloads) = fixture("reload", &["aaa"]);
        downloads.record(&track("aaa", "Aerodynamic"));

        let reloaded = Downloads::open(dir.join("downloads.jsonl"), dir.clone());
        let cached = reloaded.cached();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].track.title, "Aerodynamic");
        assert!(!cached[0].keep);
    }

    /// The file is the authority. A record whose audio has been evicted must not
    /// come back as a row that plays nothing.
    #[test]
    fn drops_entries_whose_file_is_gone() {
        let (dir, mut downloads) = fixture("gone", &["aaa", "bbb"]);
        downloads.record(&track("aaa", "Aerodynamic"));
        downloads.record(&track("bbb", "Voyager"));

        std::fs::remove_file(dir.join("bbb.m4a")).unwrap();

        let reloaded = Downloads::open(dir.join("downloads.jsonl"), dir.clone());
        let cached = reloaded.cached();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].track.video_id, "aaa");
    }

    #[test]
    fn keep_survives_a_reload_and_shows_in_the_keep_set() {
        let (dir, mut downloads) = fixture("keep", &["aaa", "bbb"]);
        downloads.record(&track("aaa", "Aerodynamic"));
        downloads.record(&track("bbb", "Voyager"));
        assert!(downloads.set_keep("aaa", true));

        let reloaded = Downloads::open(dir.join("downloads.jsonl"), dir.clone());
        let keep = reloaded.keep_set();
        assert_eq!(keep.len(), 1);
        assert!(keep.contains("aaa"));
        assert!(reloaded.is_kept("aaa"));
        assert!(!reloaded.is_kept("bbb"));

        // Recording the same track again must not quietly drop the flag.
        let mut downloads = reloaded;
        downloads.record(&track("aaa", "Aerodynamic"));
        assert!(downloads.is_kept("aaa"));
    }

    /// The file is what makes something cached, so forgetting its metadata is
    /// not the same as removing it: the file is still there and reappears as a
    /// placeholder. Removing a download does both, and then nothing remains.
    #[test]
    fn forgetting_the_metadata_leaves_a_placeholder_until_the_file_goes() {
        let (dir, mut downloads) = fixture("forget", &["aaa"]);
        downloads.record(&track("aaa", "Aerodynamic"));
        downloads.forget("aaa");

        let reloaded = Downloads::open(dir.join("downloads.jsonl"), dir.clone());
        assert_eq!(reloaded.cached().len(), 1);
        assert_eq!(reloaded.cached()[0].track.title, "aaa");

        std::fs::remove_file(dir.join("aaa.m4a")).unwrap();
        let reloaded = Downloads::open(dir.join("downloads.jsonl"), dir);
        assert!(reloaded.cached().is_empty());
    }

    /// Keeping something that was never cached is not an error, it just does
    /// nothing — the caller may be looking at a track that has been evicted.
    #[test]
    fn keeping_an_unknown_track_does_nothing() {
        let (_dir, mut downloads) = fixture("unknown", &[]);
        assert!(!downloads.set_keep("nope", true));
        assert!(downloads.keep_set().is_empty());
    }

    /// A cached file with no record still belongs in the list. Otherwise a
    /// cache filled before this index existed is invisible to the one view
    /// meant to show it.
    #[test]
    fn picks_up_files_with_no_record() {
        let (_dir, downloads) = fixture("unrecorded", &["zzz"]);
        let cached = downloads.cached();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].track.video_id, "zzz");
        assert_eq!(cached[0].track.title, "zzz", "falls back to the video id");
    }
}
