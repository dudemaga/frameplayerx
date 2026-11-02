//! The catalog shown by media-server browsers: items, pages, filters and
//! the blocking operations supplied by each server protocol.

use crate::error::{Error, Result};
use fp_core::format::VideoFormat;
use fp_core::source::{Entry, EntryKind};

/// What a library item is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CatalogKind {
    Movie,
    /// A TV series (holds seasons).
    Series,
    /// A season of a series (holds episodes).
    Season,
    /// An episode of a series.
    Episode,
    /// Any other video (home videos, mixed libraries).
    Video,
    MusicVideo,
    /// A collection of films.
    BoxSet,
    Folder,
    /// One of the user's libraries (a view).
    Library,
    /// Someone in the cast or crew.
    Person,
    /// Anything else (photos...).
    #[default]
    Other,
}

impl CatalogKind {
    /// Plays rather than opens.
    pub fn playable(self) -> bool {
        matches!(
            self,
            CatalogKind::Movie
                | CatalogKind::Episode
                | CatalogKind::Video
                | CatalogKind::MusicVideo
        )
    }

    /// Shown as a 2:3 poster rather than 16:9 art.
    pub fn portrait(self) -> bool {
        matches!(
            self,
            CatalogKind::Movie
                | CatalogKind::Series
                | CatalogKind::Season
                | CatalogKind::BoxSet
                | CatalogKind::Person
        )
    }
}

/// One catalog item, with what the server browser shows of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogItem {
    /// Server id.
    pub id: String,
    pub name: String,
    pub kind: CatalogKind,
    /// Of a library: `movies`, `tvshows`, `boxsets`, `homevideos`...
    pub collection_type: Option<String>,
    /// Plot or description.
    pub overview: Option<String>,
    /// Production year.
    pub year: Option<i32>,
    /// Length in seconds.
    pub runtime: Option<f64>,
    /// Audience rating out of 10.
    pub community_rating: Option<f32>,
    /// Age rating ("PG-13").
    pub official_rating: Option<String>,
    pub genres: Vec<String>,
    /// Tags (details only).
    pub tags: Vec<String>,
    /// Studios (details only).
    pub studios: Vec<String>,
    /// Cast and crew as (name, role), actors first (details only).
    pub people: Vec<(String, String)>,
    /// Series of an episode or season.
    pub series_id: Option<String>,
    /// Name of that series.
    pub series_name: Option<String>,
    /// Season of an episode.
    pub season_id: Option<String>,
    /// Episode number, or a season's number.
    pub index: Option<u32>,
    /// Season number of an episode.
    pub parent_index: Option<u32>,
    /// Items inside (seasons of a series, films of a collection).
    pub child_count: Option<u32>,
    /// Watched.
    pub played: bool,
    /// Resume position in seconds (0 when not started).
    pub position: f64,
    /// The user's favourite.
    pub favorite: bool,
    /// Unwatched episodes of a series or season.
    pub unplayed: Option<u32>,
    /// When it was added, Unix seconds.
    pub added: Option<i64>,
    /// 2:3 poster (an episode's still for episodes).
    pub poster: Option<String>,
    /// 16:9 art for rows of landscape cards.
    pub landscape: Option<String>,
    /// Large wide art for the top of a details page.
    pub backdrop: Option<String>,
    /// Location for [`Source::open`](crate::Source::open) and browsing.
    pub location: String,
    /// Projection and stereo layout from the file name or 3D metadata.
    pub format: Option<VideoFormat>,
    /// The files it can be played from (details only): one, or several
    /// when the server keeps more than one version (4K and 1080p, a cut).
    pub versions: Vec<CatalogVersion>,
    /// The version chosen with [`Catalog::set_version`]
    /// (details only); `None` plays the tallest that fits the source's
    /// maximum height.
    pub version: Option<String>,
}

/// One version of a film or video: a media source of the item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogVersion {
    /// The server version id, for [`Catalog::set_version`].
    pub id: String,
    /// The server's name for it ("2160p", "Director's Cut").
    pub name: String,
    /// Picture height in pixels.
    pub height: Option<u32>,
    /// Video codec ("hevc").
    pub codec: Option<String>,
    /// Dynamic range other than SDR ("HDR", "DOVI").
    pub range: Option<String>,
    /// File size in bytes.
    pub size: Option<u64>,
    /// Projection and stereo layout from its file name or 3D metadata.
    pub format: Option<VideoFormat>,
}

impl CatalogVersion {
    /// What tells it apart: "2160p · HEVC · HDR · 18.2 GB", after the
    /// server's name when that says something more.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        let height = self.height.map(|h| format!("{h}p"));
        if !self.name.is_empty() && height.as_deref() != Some(self.name.as_str()) {
            parts.push(self.name.clone());
        }
        parts.extend(height);
        parts.extend(self.codec.as_ref().map(|c| c.to_uppercase()));
        parts.extend(self.range.clone());
        parts.extend(self.size.map(|b| format!("{:.1} GB", b as f64 / 1e9)));
        parts.join(" · ")
    }
}

impl CatalogItem {
    /// Fraction watched when partly watched.
    pub fn progress(&self) -> Option<f32> {
        let d = self.runtime.filter(|d| *d > 0.0)?;
        (self.position > 0.0).then(|| (self.position / d).clamp(0.0, 1.0) as f32)
    }

    /// "S1:E3" for an episode.
    pub fn episode_label(&self) -> Option<String> {
        match (self.parent_index, self.index) {
            (Some(s), Some(e)) => Some(format!("S{s}:E{e}")),
            (None, Some(e)) => Some(format!("E{e}")),
            _ => None,
        }
    }

    /// The format of the version chosen, else the item's own.
    pub fn shown_format(&self) -> Option<VideoFormat> {
        self.version
            .as_ref()
            .and_then(|id| self.versions.iter().find(|v| &v.id == id))
            .and_then(|v| v.format)
            .or(self.format)
    }

    /// The name to remember it by: episodes carry their series and number.
    pub fn full_title(&self) -> String {
        match (&self.series_name, self.episode_label()) {
            (Some(series), Some(ep)) if self.kind == CatalogKind::Episode => {
                format!("{series} {ep} · {}", self.name)
            }
            _ => self.name.clone(),
        }
    }

    /// A browse entry to open it with.
    pub fn to_entry(&self) -> Entry {
        let kind = if self.kind.playable() {
            EntryKind::Video
        } else {
            EntryKind::Directory
        };
        let mut e = Entry::new(self.full_title(), self.location.clone(), kind);
        e.duration = self.runtime;
        e.thumbnail_url = self.landscape.clone().or_else(|| self.poster.clone());
        e.modified = self.added;
        e.format = self.format;
        e
    }
}

/// Order of a listing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CatalogSort {
    /// By title.
    #[default]
    Name,
    /// Newest additions first.
    Added,
    /// By release date.
    Released,
    /// By audience rating.
    Rating,
    /// By length.
    Runtime,
    /// Most recently watched first.
    LastWatched,
    /// Shuffled.
    Random,
}

impl CatalogSort {
    /// Whether this order reads best largest first.
    pub fn default_descending(self) -> bool {
        matches!(
            self,
            CatalogSort::Added
                | CatalogSort::Released
                | CatalogSort::Rating
                | CatalogSort::LastWatched
        )
    }
}

/// Which items a listing keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CatalogFilter {
    /// Everything.
    #[default]
    All,
    /// Not watched yet.
    Unplayed,
    /// Partly watched.
    Resumable,
    /// The user's favourites.
    Favorites,
}

/// A server-side listing: search, sort, filter and paging.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogQuery {
    /// Library, folder or collection to list; `None` for the whole server.
    pub parent: Option<String>,
    /// Item kinds to keep; empty keeps every kind.
    pub types: Vec<CatalogKind>,
    /// Look inside sub-folders too.
    pub recursive: bool,
    /// Words to search for; empty lists everything.
    pub search: String,
    pub sort: CatalogSort,
    /// Largest first.
    pub descending: bool,
    /// Which items to keep.
    pub filter: CatalogFilter,
    /// Items to skip (paging).
    pub start: usize,
    /// Items per page.
    pub limit: usize,
    /// Only items with one of these tags.
    pub tags: Vec<String>,
    /// Only items in one of these genres.
    pub genres: Vec<String>,
    /// Only items from one of these years.
    pub years: Vec<i32>,
    /// Only items from one of these studios.
    pub studios: Vec<String>,
    /// Only videos at least this many pixels wide.
    pub min_width: Option<u32>,
    /// Only items this person is in, by id (one: Jellyfin and Emby separate
    /// several differently).
    pub person: Option<String>,
    /// These items, in this order (a page of a shuffle); the other fields
    /// still choose what is listed.
    pub ids: Vec<String>,
}

impl Default for CatalogQuery {
    fn default() -> CatalogQuery {
        CatalogQuery {
            parent: None,
            types: Vec::new(),
            recursive: false,
            search: String::new(),
            sort: CatalogSort::Name,
            descending: false,
            filter: CatalogFilter::All,
            start: 0,
            limit: 60,
            tags: Vec::new(),
            genres: Vec::new(),
            years: Vec::new(),
            studios: Vec::new(),
            min_width: None,
            person: None,
            ids: Vec::new(),
        }
    }
}

impl CatalogQuery {
    /// Everything on the server with `tag`, by name.
    pub fn tagged(tag: &str) -> CatalogQuery {
        CatalogQuery {
            types: vec![
                CatalogKind::Movie,
                CatalogKind::Series,
                CatalogKind::Episode,
                CatalogKind::Video,
                CatalogKind::MusicVideo,
            ],
            recursive: true,
            tags: vec![tag.to_string()],
            ..CatalogQuery::default()
        }
    }

    /// What opening `item` lists: a film library its films, a TV library
    /// its series, a collection or folder what is in it, a person what they
    /// are in.
    pub fn children(item: &CatalogItem) -> CatalogQuery {
        if item.kind == CatalogKind::Person {
            return CatalogQuery {
                types: vec![
                    CatalogKind::Movie,
                    CatalogKind::Series,
                    CatalogKind::Episode,
                    CatalogKind::Video,
                    CatalogKind::MusicVideo,
                ],
                recursive: true,
                person: Some(item.id.clone()),
                ..CatalogQuery::default()
            };
        }
        let types: &[CatalogKind] = match (item.kind, item.collection_type.as_deref()) {
            (CatalogKind::Library, Some("movies")) => &[CatalogKind::Movie],
            (CatalogKind::Library, Some("tvshows")) => &[CatalogKind::Series],
            (CatalogKind::Library, Some("boxsets")) => &[CatalogKind::BoxSet],
            (CatalogKind::Library, Some("homevideos")) => &[CatalogKind::Video],
            (CatalogKind::Library, Some("musicvideos")) => &[CatalogKind::MusicVideo],
            (CatalogKind::Library, _) => &[
                CatalogKind::Movie,
                CatalogKind::Series,
                CatalogKind::Video,
                CatalogKind::MusicVideo,
            ],
            _ => &[],
        };
        CatalogQuery {
            parent: Some(item.id.clone()),
            types: types.to_vec(),
            recursive: item.kind == CatalogKind::Library,
            ..CatalogQuery::default()
        }
    }
}

/// What a listing can be filtered by: the genres, years, studios and tags
/// its items have.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogFacets {
    pub genres: Vec<String>,
    /// Newest first.
    pub years: Vec<i32>,
    pub studios: Vec<String>,
    pub tags: Vec<String>,
}

/// One page of a listing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogPage {
    /// Items on this page.
    pub items: Vec<CatalogItem>,
    /// Items in the whole listing.
    pub total: usize,
}

/// A server's front page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogHome {
    /// The user's video libraries.
    pub libraries: Vec<CatalogItem>,
    /// Partly watched films and episodes.
    pub resume: Vec<CatalogItem>,
    /// The next episode of each series being watched.
    pub next_up: Vec<CatalogItem>,
    /// Newest additions, per library.
    pub latest: Vec<(CatalogItem, Vec<CatalogItem>)>,
}

/// Features a server can apply to the shared catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogCapabilities {
    pub favorites: bool,
    pub width_filter: bool,
    pub people: bool,
    pub tags: bool,
}

impl Default for CatalogCapabilities {
    fn default() -> Self {
        Self {
            favorites: true,
            width_filter: true,
            people: true,
            tags: true,
        }
    }
}

/// Blocking catalog operations. Call these on a job thread, never while
/// building a frame. Each protocol maps neutral kinds and fields itself.
pub trait Catalog: Send + Sync {
    fn capabilities(&self) -> CatalogCapabilities {
        CatalogCapabilities::default()
    }
    fn home(&self) -> Result<CatalogHome>;
    fn search(&self, text: &str, limit: usize) -> Result<Vec<(&'static str, Vec<CatalogItem>)>>;
    fn items(&self, query: &CatalogQuery) -> Result<CatalogPage>;
    fn shuffled(&self, query: &CatalogQuery) -> Result<Vec<String>>;
    fn facets(&self, query: &CatalogQuery) -> Result<CatalogFacets>;
    fn item(&self, id: &str) -> Result<CatalogItem>;
    fn seasons(&self, series: &str) -> Result<Vec<CatalogItem>>;
    fn next_up(&self, series: Option<&str>, limit: usize) -> Result<Vec<CatalogItem>>;
    fn episodes(&self, series: &str, season: Option<&str>) -> Result<Vec<CatalogItem>>;
    fn set_favorite(&self, _id: &str, _favorite: bool) -> Result<()> {
        Err(Error::Unsupported("server favorites".into()))
    }
    fn set_played(&self, id: &str, played: bool) -> Result<()>;
    fn set_version(&self, location: &str, version: Option<&str>);
}
