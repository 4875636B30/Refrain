//! Clean domain types, decoupled from YouTube's deeply-nested renderer JSON.

/// How the signed-in listener rated a track.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LikeStatus {
    #[default]
    Indifferent,
    Like,
    Dislike,
}

impl LikeStatus {
    /// The status a thumb click moves to. Clicking the rating that is already
    /// active clears it, which is how YouTube Music's thumbs behave.
    pub fn toggled(self, clicked: Self) -> Self {
        if self == clicked {
            Self::Indifferent
        } else {
            clicked
        }
    }
}

/// A playable song / video.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub video_id: String,
    pub title: String,
    pub artists: Vec<String>,
    /// Browse ids parallel to `artists`, so names can be opened.
    pub artist_ids: Vec<String>,
    pub album: Option<String>,
    pub duration_secs: Option<u32>,
    /// The id of this track's *entry* in the playlist it came from, when it came
    /// from one. YouTube Music identifies entries this way rather than by video
    /// id, because the same track can appear in a playlist more than once —
    /// removing it needs this, not the video id.
    pub set_video_id: Option<String>,
    pub thumbnail: Option<String>,
}

impl Track {
    pub fn artist_line(&self) -> String {
        if self.artists.is_empty() {
            "Unknown artist".to_string()
        } else {
            self.artists.join(", ")
        }
    }

    pub fn duration_text(&self) -> Option<String> {
        self.duration_secs.map(fmt_duration)
    }
}

pub fn fmt_duration(secs: u32) -> String {
    let (m, s) = (secs / 60, secs % 60);
    format!("{m}:{s:02}")
}

/// What kind of detail page a `browseId` points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    Album,
    Playlist,
    Artist,
    Podcast,
    Unknown,
}

impl PageKind {
    pub fn label(self) -> &'static str {
        match self {
            PageKind::Album => "Album",
            PageKind::Playlist => "Playlist",
            PageKind::Artist => "Artist",
            PageKind::Podcast => "Podcast",
            PageKind::Unknown => "",
        }
    }
}

/// A track's lyrics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lyrics {
    pub text: String,
    /// Who the words are licensed from, as the page credits it.
    pub source: Option<String>,
}

/// A titled group of browsable tiles (albums, playlists, artists…).
#[derive(Clone, Debug)]
pub struct Shelf {
    pub title: String,
    pub items: Vec<SearchItem>,
}

/// A fully-resolved detail page (album, playlist, artist).
#[derive(Clone, Debug)]
pub struct Page {
    pub browse_id: String,
    pub kind: PageKind,
    pub title: String,
    pub subtitle: String,
    pub description: String,
    pub thumbnail: Option<String>,
    pub tracks: Vec<Track>,
    pub shelves: Vec<Shelf>,
    /// Token for loading further shelves (the home/explore feeds page in).
    pub continuation: Option<String>,
    /// How many tracks the page claims to have, if it says.
    pub track_count: Option<usize>,
    /// Raw playlist id, for paging long track lists via the player panel.
    pub playlist_id: Option<String>,
    /// Whether a person, rather than YouTube Music itself, is credited as this
    /// playlist's owner. A missing marker reads as "editorial" — the safe
    /// direction, since it only hides the destructive action.
    pub owner_is_user: bool,
}

impl Page {
    /// Whether the page is showing fewer tracks than it has.
    pub fn has_more_tracks(&self) -> bool {
        self.playlist_id.is_some() && self.track_count.is_some_and(|n| n > self.tracks.len())
    }
}

/// Classification of a single search result row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Song,
    Video,
    Album,
    Artist,
    Playlist,
    Podcast,
    Profile,
    Other,
}

impl ItemKind {
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Song => "Song",
            ItemKind::Video => "Video",
            ItemKind::Album => "Album",
            ItemKind::Artist => "Artist",
            ItemKind::Playlist => "Playlist",
            ItemKind::Podcast => "Podcast",
            ItemKind::Profile => "Profile",
            ItemKind::Other => "",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchItem {
    pub title: String,
    pub artists: Vec<String>,
    /// Full secondary line, e.g. `Daft Punk • Random Access Memories • 5:38`.
    pub subtitle: String,
    /// The subtitle split into parts; `Some(id)` marks a clickable link.
    pub segments: Vec<(String, Option<String>)>,
    pub kind: ItemKind,
    pub browse_id: Option<String>,
    pub video_id: Option<String>,
    pub thumbnail: Option<String>,
    pub duration_secs: Option<u32>,
}

impl SearchItem {
    /// A result row is directly playable when it is a song with a video id.
    pub fn playable(&self) -> bool {
        matches!(self.kind, ItemKind::Song) && self.video_id.is_some()
    }

    /// A result row is navigable when it carries a browse id.
    pub fn navigable(&self) -> bool {
        self.browse_id.is_some() && !matches!(self.kind, ItemKind::Song)
    }

    pub fn to_track(&self) -> Option<Track> {
        Some(Track {
            video_id: self.video_id.clone()?,
            title: self.title.clone(),
            artists: self.artists.clone(),
            artist_ids: self
                .segments
                .iter()
                .filter(|(_, link)| link.is_some())
                .filter_map(|(name, link)| {
                    self.artists.contains(name).then(|| link.clone()).flatten()
                })
                .collect(),
            album: None,
            duration_secs: self.duration_secs,
            // Search results are not playlist entries.
            set_video_id: None,
            thumbnail: self.thumbnail.clone(),
        })
    }

    /// Present a playable track as a grid/search item.
    pub fn from_track(track: &Track) -> Self {
        Self {
            title: track.title.clone(),
            artists: track.artists.clone(),
            subtitle: track.artist_line(),
            segments: Vec::new(),
            kind: ItemKind::Song,
            browse_id: None,
            video_id: Some(track.video_id.clone()),
            thumbnail: track.thumbnail.clone(),
            duration_secs: track.duration_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LikeStatus::{Dislike, Indifferent, Like};

    /// Clicking the active thumb clears the rating; clicking the other one
    /// switches to it. This is the whole toggle behaviour in one place.
    #[test]
    fn thumbs_toggle_the_way_youtube_music_does() {
        assert_eq!(Indifferent.toggled(Like), Like);
        assert_eq!(Indifferent.toggled(Dislike), Dislike);
        assert_eq!(Like.toggled(Like), Indifferent);
        assert_eq!(Dislike.toggled(Dislike), Indifferent);
        assert_eq!(Like.toggled(Dislike), Dislike);
        assert_eq!(Dislike.toggled(Like), Like);
    }
}
