//! What a Jellyfin/Emby client shows: libraries, posters, series and their
//! seasons, resume points, "next up" and favourites, read with the session
//! [`MediaBrowserSource`] browses with.
//!
//! Listings ask the server to search, sort, filter and page
//! (`/Users/{id}/Items`), so a library of thousands of films costs one small
//! request per page.

use super::{
    MediaBrowserSource, detect_from_name, encode_location, ticks_to_seconds, video_3d_format,
};
use crate::Source;
use crate::error::{Error, Result};
use crate::json::{array, boolean, get, num, text};
use crate::timeutil::parse_iso8601;
use crate::urlutil::encode_form_value;
use fp_core::sync::lock;
use serde_json::Value;

/// Fields asked for in listings, on top of those always sent (name, type,
/// user data, image tags, run time, ratings, episode numbers).
const LIST_FIELDS: &str = "Overview,Genres,Path,DateCreated,ChildCount,PremiereDate";

/// Fields asked for one item's details.
const DETAIL_FIELDS: &str =
    "Overview,Genres,Tags,Studios,People,Path,DateCreated,ChildCount,PremiereDate,MediaSources";

/// What a tag can be on, and a person in: films, series, episodes and
/// other videos.
const TAGGED: [&str; 5] = ["Movie", "Series", "Episode", "Video", "MusicVideo"];

/// Image types listings ask tags for.
const IMAGES: &str = "EnableImageTypes=Primary,Backdrop,Thumb&ImageTypeLimit=1";

/// Library kinds (`CollectionType`) without video.
const NOT_VIDEO: [&str; 6] = [
    "music",
    "books",
    "photos",
    "livetv",
    "playlists",
    "audiobooks",
];

pub use crate::catalog::{
    CatalogFacets as MbFacets, CatalogFilter as MbFilter, CatalogHome as MbHome,
    CatalogItem as MbItem, CatalogKind as MbKind, CatalogPage as MbPage, CatalogQuery as MbQuery,
    CatalogSort as MbSort, CatalogVersion as MbVersion,
};

impl MbKind {
    fn parse(t: &str) -> MbKind {
        match t {
            "Movie" => MbKind::Movie,
            "Series" => MbKind::Series,
            "Season" => MbKind::Season,
            "Episode" => MbKind::Episode,
            "Video" => MbKind::Video,
            "MusicVideo" => MbKind::MusicVideo,
            "BoxSet" => MbKind::BoxSet,
            "Folder" => MbKind::Folder,
            "CollectionFolder" | "UserView" => MbKind::Library,
            "Person" => MbKind::Person,
            _ => MbKind::Other,
        }
    }
}

fn kind_api(kind: MbKind) -> &'static str {
    match kind {
        MbKind::Movie => "Movie",
        MbKind::Series => "Series",
        MbKind::Season => "Season",
        MbKind::Episode => "Episode",
        MbKind::Video => "Video",
        MbKind::MusicVideo => "MusicVideo",
        MbKind::BoxSet => "BoxSet",
        MbKind::Folder => "Folder",
        MbKind::Library => "CollectionFolder",
        MbKind::Person => "Person",
        MbKind::Other => "Other",
    }
}

fn type_names(types: &[MbKind]) -> String {
    types
        .iter()
        .map(|kind| kind_api(*kind))
        .collect::<Vec<_>>()
        .join(",")
}

/// The versions in an item's `MediaSources`.
fn parse_versions(v: &Value) -> Vec<MbVersion> {
    array(v, "MediaSources")
        .iter()
        .filter_map(|ms| {
            let video = array(ms, "MediaStreams")
                .iter()
                .find(|s| text(s, "Type").as_deref() == Some("Video"));
            Some(MbVersion {
                id: text(ms, "Id")?,
                name: text(ms, "Name").unwrap_or_default(),
                height: super::media_source_height(ms),
                codec: video.and_then(|s| text(s, "Codec")),
                range: video
                    .and_then(|s| text(s, "VideoRangeType").or_else(|| text(s, "VideoRange")))
                    .filter(|r| {
                        !r.eq_ignore_ascii_case("SDR") && !r.eq_ignore_ascii_case("Unknown")
                    }),
                size: num(ms, "Size").filter(|s| *s > 0.0).map(|s| s as u64),
                format: text(ms, "Path")
                    .and_then(|p| detect_from_name(&p))
                    .or_else(|| video_3d_format(ms)),
            })
        })
        .collect()
}

impl MbSort {
    fn api(self) -> &'static str {
        match self {
            MbSort::Name => "SortName",
            MbSort::Added => "DateCreated,SortName",
            MbSort::Released => "PremiereDate,ProductionYear,SortName",
            MbSort::Rating => "CommunityRating,SortName",
            MbSort::Runtime => "Runtime,SortName",
            MbSort::LastWatched => "DatePlayed,SortName",
            MbSort::Random => "Random",
        }
    }
}

impl MbFilter {
    fn api(self) -> Option<&'static str> {
        match self {
            MbFilter::All => None,
            MbFilter::Unplayed => Some("IsUnplayed"),
            MbFilter::Resumable => Some("IsResumable"),
            MbFilter::Favorites => Some("IsFavorite"),
        }
    }
}

impl MbQuery {
    /// Path and query string of `/Users/{user}/Items` for this listing.
    pub(crate) fn path(&self, user: &str) -> String {
        let mut q = format!(
            "/Users/{user}/Items?Recursive={}&SortBy={}&SortOrder={}&StartIndex={}&Limit={}\
             &Fields={LIST_FIELDS}&{IMAGES}&EnableTotalRecordCount=true",
            self.recursive,
            self.sort.api(),
            if self.descending {
                "Descending"
            } else {
                "Ascending"
            },
            self.start,
            self.limit.max(1),
        );
        if let Some(p) = &self.parent {
            q += &format!("&ParentId={}", encode_form_value(p));
        }
        if !self.types.is_empty() {
            q += &format!("&IncludeItemTypes={}", type_names(&self.types));
        }
        if let Some(f) = self.filter.api() {
            q += &format!("&Filters={f}");
        }
        let search = self.search.trim();
        if !search.is_empty() {
            q += &format!("&SearchTerm={}", encode_form_value(search));
        }
        q + &self.filter_params()
    }

    /// The listing's every item, shuffled by the server, as ids alone: what
    /// a shuffle pages through, so each item comes once.
    pub(crate) fn shuffled_path(&self, user: &str) -> String {
        let mut q = format!(
            "/Users/{user}/Items?Recursive={}&SortBy=Random&EnableImages=false\
             &EnableUserData=false&EnableTotalRecordCount=false",
            self.recursive,
        );
        if let Some(p) = &self.parent {
            q += &format!("&ParentId={}", encode_form_value(p));
        }
        if !self.types.is_empty() {
            q += &format!("&IncludeItemTypes={}", type_names(&self.types));
        }
        if let Some(f) = self.filter.api() {
            q += &format!("&Filters={f}");
        }
        let search = self.search.trim();
        if !search.is_empty() {
            q += &format!("&SearchTerm={}", encode_form_value(search));
        }
        q + &self.filter_params()
    }

    /// The tags, genres, years, studios, width and ids asked for.
    fn filter_params(&self) -> String {
        let mut q = String::new();
        let any_of = |values: &[String]| encode_form_value(&values.join("|"));
        if !self.tags.is_empty() {
            q += &format!("&Tags={}", any_of(&self.tags));
        }
        if !self.genres.is_empty() {
            q += &format!("&Genres={}", any_of(&self.genres));
        }
        if !self.studios.is_empty() {
            q += &format!("&Studios={}", any_of(&self.studios));
        }
        if !self.years.is_empty() {
            let years: Vec<String> = self.years.iter().map(ToString::to_string).collect();
            q += &format!("&Years={}", years.join(","));
        }
        if let Some(w) = self.min_width {
            q += &format!("&MinWidth={w}");
        }
        if let Some(p) = &self.person {
            q += &format!("&PersonIds={}", encode_form_value(p));
        }
        if !self.ids.is_empty() {
            q += &format!("&Ids={}", self.ids.join(","));
        }
        q
    }

    /// The query string a listing's genres, years, studios and tags are
    /// asked for with: its library and kinds of item.
    fn scope_params(&self, user: &str) -> String {
        let mut q =
            format!("UserId={user}&Recursive=true&EnableImages=false&EnableTotalRecordCount=false");
        if let Some(p) = &self.parent {
            q += &format!("&ParentId={}", encode_form_value(p));
        }
        let types = if self.types.is_empty() {
            TAGGED.join(",")
        } else {
            type_names(&self.types)
        };
        q + &format!("&IncludeItemTypes={types}")
    }
}

/// The names of a `{"Items": [{"Name": ...}]}` answer, as `/Genres`,
/// `/Studios`, `/Years` and `/Tags` give them.
fn names(v: &Value) -> Vec<String> {
    let mut names: Vec<String> = array(v, "Items")
        .iter()
        .filter_map(|i| text(i, "Name"))
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

/// Ids in the order of an `{"Items": [{"Id": ...}]}` answer.
fn ids(v: &Value) -> Vec<String> {
    array(v, "Items")
        .iter()
        .filter_map(|i| text(i, "Id"))
        .collect()
}

/// `items` in the order of `ids`; ones the server didn't send are left out.
fn in_order(mut items: Vec<MbItem>, ids: &[String]) -> Vec<MbItem> {
    items.retain(|i| ids.contains(&i.id));
    items.sort_by_key(|item| ids.iter().position(|id| *id == item.id));
    items
}

/// What a search of the whole server looks through, a section each, as
/// Jellyfin's own search shows it: the titles of films, series, collections
/// and other videos. Episodes are left out; a series is found by its name.
pub(crate) const SEARCH_SECTIONS: [(&str, &[&str]); 4] = [
    ("Movies", &["Movie"]),
    ("Shows", &["Series"]),
    ("Collections", &["BoxSet"]),
    ("Videos", &["Video", "MusicVideo"]),
];

/// The tags (at most 20) that have every word of `text` in them, ignoring
/// case.
pub(crate) fn matching_tags(tags: &[String], text: &str) -> Vec<String> {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Vec::new();
    }
    tags.iter()
        .filter(|t| {
            let t = t.to_lowercase();
            words.iter().all(|w| t.contains(w.as_str()))
        })
        .take(20)
        .cloned()
        .collect()
}

/// Path and query string of one section of a search. It asks for no order,
/// as Jellyfin's web client doesn't, so the server ranks the matches itself:
/// exact and leading matches first.
pub(crate) fn search_path(user: &str, text: &str, types: &[&str], limit: usize) -> String {
    format!(
        "/Users/{user}/Items?SearchTerm={}&IncludeItemTypes={}&Recursive=true&Limit={}\
         &Fields={LIST_FIELDS}&{IMAGES}&EnableTotalRecordCount=false",
        encode_form_value(text.trim()),
        types.join(","),
        limit.max(1),
    )
}

/// The people whose names have `text` in them.
pub(crate) fn people_search_path(user: &str, text: &str, limit: usize) -> String {
    format!(
        "/Persons?UserId={user}&SearchTerm={}&Limit={}&{IMAGES}&EnableTotalRecordCount=false",
        encode_form_value(text.trim()),
        limit.max(1),
    )
}

/// `{base}/Items/{id}/Images/{kind}?...`
fn image_url(base: &str, id: &str, kind: &str, tag: Option<&str>, size: &str) -> String {
    let mut u = format!("{base}/Items/{id}/Images/{kind}?{size}&quality=90");
    if let Some(t) = tag {
        u += &format!("&tag={}", encode_form_value(t));
    }
    u
}

const POSTER: &str = "maxHeight=480";
const LANDSCAPE: &str = "maxWidth=640";
const BACKDROP: &str = "maxWidth=1280";

/// Parses one item. Images follow what the item has, then its parents'
/// (an episode without a still shows its series' poster).
pub(crate) fn parse_item(v: &Value, base: &str) -> Option<MbItem> {
    let id = text(v, "Id")?;
    let name = text(v, "Name").unwrap_or_default();
    let kind = MbKind::parse(&text(v, "Type").unwrap_or_default());
    let user = get(v, "UserData");
    let tags = get(v, "ImageTags");
    // Servers that send no tags at all get the plain primary image.
    let own_primary = match tags {
        None => Some(None),
        Some(t) => text(t, "Primary").map(Some),
    };
    let own_thumb = tags.and_then(|t| text(t, "Thumb"));
    let backdrop_tag = array(v, "BackdropImageTags")
        .first()
        .and_then(|t| t.as_str())
        .map(str::to_string);
    let parent_backdrop = text(v, "ParentBackdropItemId").map(|pid| {
        let tag = array(v, "ParentBackdropImageTags")
            .first()
            .and_then(|t| t.as_str())
            .map(str::to_string);
        (pid, tag)
    });

    let poster = own_primary
        .as_ref()
        .map(|tag| image_url(base, &id, "Primary", tag.as_deref(), POSTER))
        .or_else(|| {
            let sid = text(v, "SeriesId")?;
            let tag = text(v, "SeriesPrimaryImageTag")?;
            Some(image_url(base, &sid, "Primary", Some(&tag), POSTER))
        });
    let backdrop = backdrop_tag
        .as_ref()
        .map(|tag| image_url(base, &id, "Backdrop/0", Some(tag), BACKDROP))
        .or_else(|| {
            let (pid, tag) = parent_backdrop.as_ref()?;
            Some(image_url(base, pid, "Backdrop/0", tag.as_deref(), BACKDROP))
        });
    let landscape = if kind == MbKind::Episode && own_primary.is_some() {
        poster.clone()
    } else {
        own_thumb
            .map(|tag| image_url(base, &id, "Thumb", Some(&tag), LANDSCAPE))
            .or_else(|| {
                let pid = text(v, "ParentThumbItemId")?;
                let tag = text(v, "ParentThumbImageTag");
                Some(image_url(base, &pid, "Thumb", tag.as_deref(), LANDSCAPE))
            })
            .or_else(|| {
                backdrop_tag
                    .as_ref()
                    .map(|tag| image_url(base, &id, "Backdrop/0", Some(tag), LANDSCAPE))
            })
            .or_else(|| {
                let (pid, tag) = parent_backdrop.as_ref()?;
                Some(image_url(
                    base,
                    pid,
                    "Backdrop/0",
                    tag.as_deref(),
                    LANDSCAPE,
                ))
            })
            .or_else(|| (!kind.portrait()).then(|| poster.clone()).flatten())
    };

    // (actor, name, role): actors with their part, crew with their job.
    let mut people: Vec<(bool, String, String)> = array(v, "People")
        .iter()
        .filter_map(|p| {
            let name = text(p, "Name")?;
            let job = text(p, "Type");
            let actor = job.as_deref().is_none_or(|t| t == "Actor");
            let role = text(p, "Role").filter(|_| actor).or(job);
            Some((actor, name, role.unwrap_or_default()))
        })
        .collect();
    // Actors first, each group in the server's order.
    people.sort_by_key(|p| !p.0);

    let path = text(v, "Path").unwrap_or_default();
    let format = kind
        .playable()
        .then(|| {
            detect_from_name(&path)
                .or_else(|| detect_from_name(&name))
                .or_else(|| video_3d_format(v))
        })
        .flatten();
    let location = if kind.playable() {
        encode_location(base, "item", &id)
    } else {
        encode_location(base, "folder", &id)
    };
    let year = num(v, "ProductionYear")
        .filter(|y| *y > 0.0 && *y < 10000.0)
        .map(|y| y as i32);
    Some(MbItem {
        name,
        kind,
        collection_type: text(v, "CollectionType"),
        overview: text(v, "Overview"),
        year,
        runtime: num(v, "RunTimeTicks")
            .filter(|t| *t > 0.0)
            .map(ticks_to_seconds),
        community_rating: num(v, "CommunityRating")
            .filter(|r| *r > 0.0)
            .map(|r| r as f32),
        official_rating: text(v, "OfficialRating"),
        genres: strings(v, "Genres"),
        tags: tag_names(v),
        studios: array(v, "Studios")
            .iter()
            .filter_map(|s| text(s, "Name").or_else(|| s.as_str().map(str::to_string)))
            .collect(),
        people: people.into_iter().map(|(_, n, r)| (n, r)).collect(),
        series_id: text(v, "SeriesId"),
        series_name: text(v, "SeriesName"),
        season_id: text(v, "SeasonId"),
        index: num(v, "IndexNumber")
            .filter(|n| *n >= 0.0)
            .map(|n| n as u32),
        parent_index: num(v, "ParentIndexNumber")
            .filter(|n| *n >= 0.0)
            .map(|n| n as u32),
        child_count: num(v, "ChildCount").filter(|n| *n >= 0.0).map(|n| n as u32),
        played: user.and_then(|u| boolean(u, "Played")).unwrap_or(false),
        position: user
            .and_then(|u| num(u, "PlaybackPositionTicks"))
            .filter(|t| *t > 0.0)
            .map(ticks_to_seconds)
            .unwrap_or(0.0),
        favorite: user.and_then(|u| boolean(u, "IsFavorite")).unwrap_or(false),
        unplayed: user
            .and_then(|u| num(u, "UnplayedItemCount"))
            .filter(|n| *n >= 0.0)
            .map(|n| n as u32),
        added: text(v, "DateCreated").and_then(|d| parse_iso8601(&d)),
        poster,
        landscape,
        backdrop,
        location,
        format,
        versions: parse_versions(v),
        version: None,
        id,
    })
}

pub(super) fn strings(v: &Value, key: &str) -> Vec<String> {
    array(v, key)
        .iter()
        .filter_map(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// An item's tags: Jellyfin's `Tags`, or Emby's `TagItems` (each a name
/// and an id).
pub(super) fn tag_names(v: &Value) -> Vec<String> {
    let tags = strings(v, "Tags");
    if !tags.is_empty() {
        return tags;
    }
    array(v, "TagItems")
        .iter()
        .filter_map(|t| text(t, "Name"))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Items of a `{"Items": [...]}` answer, or of a bare array (`/Latest`).
pub(crate) fn parse_list(v: &Value, base: &str) -> Vec<MbItem> {
    let items = match v {
        Value::Array(a) => a.as_slice(),
        _ => array(v, "Items"),
    };
    items.iter().filter_map(|i| parse_item(i, base)).collect()
}

/// A page of a `{"Items": [...], "TotalRecordCount": n}` answer.
pub(crate) fn parse_page(v: &Value, base: &str) -> MbPage {
    let items = parse_list(v, base);
    let total = num(v, "TotalRecordCount")
        .map(|n| n.max(0.0) as usize)
        .unwrap_or(items.len());
    MbPage {
        total: total.max(items.len()),
        items,
    }
}

/// The user's libraries that hold video.
pub(crate) fn video_libraries(items: Vec<MbItem>) -> Vec<MbItem> {
    items
        .into_iter()
        .filter(|i| {
            i.collection_type
                .as_deref()
                .is_none_or(|t| !NOT_VIDEO.contains(&t))
        })
        .map(|mut i| {
            i.kind = MbKind::Library;
            i
        })
        .collect()
}

impl MediaBrowserSource {
    fn stable_item(&self, item: &mut MbItem) {
        if let Some(location) = self.canonical_location(&item.location) {
            item.location = location;
        }
        for asset in [&mut item.poster, &mut item.landscape, &mut item.backdrop] {
            if let Some(url) = asset.as_mut()
                && let Some(stable) = self.stable_asset(url)
            {
                *url = stable;
            }
        }
    }

    fn stable_items(&self, mut items: Vec<MbItem>) -> Vec<MbItem> {
        for item in &mut items {
            self.stable_item(item);
        }
        items
    }

    fn user(&self) -> Result<String> {
        Ok(self.client()?.1)
    }

    /// The user's video libraries.
    pub fn libraries(&self) -> Result<Vec<MbItem>> {
        let user = self.user()?;
        let v = self.api_get_json(&format!("/Users/{user}/Views?{IMAGES}"))?;
        Ok(self.stable_items(video_libraries(parse_list(&v, &self.base_url))))
    }

    /// One page of a listing; a page of a shuffle (`query.ids`) in the
    /// shuffle's order.
    pub fn items(&self, query: &MbQuery) -> Result<MbPage> {
        let user = self.user()?;
        let v = self.api_get_json(&query.path(&user))?;
        let mut page = parse_page(&v, &self.base_url);
        if !query.ids.is_empty() {
            page.items = in_order(page.items, &query.ids);
        }
        page.items = self.stable_items(page.items);
        Ok(page)
    }

    /// Every item of a listing in a new random order, by id.
    pub fn shuffled(&self, query: &MbQuery) -> Result<Vec<String>> {
        let user = self.user()?;
        let v = self.api_get_json(&query.shuffled_path(&user))?;
        Ok(ids(&v))
    }

    /// The genres, years, studios and tags of a listing's library, to
    /// filter it by. Emby has no `/Items/Filters`; its tags are a list of
    /// their own, as are both servers' genres, studios and years.
    pub fn facets(&self, query: &MbQuery) -> Result<MbFacets> {
        let user = self.user()?;
        let scope = query.scope_params(&user);
        let list = |kind: &str| -> Result<Vec<String>> {
            Ok(names(&self.api_get_json(&format!("/{kind}?{scope}"))?))
        };
        let mut years: Vec<i32> = list("Years")?
            .iter()
            .filter_map(|y| y.parse().ok())
            .collect();
        years.sort_unstable_by(|a, b| b.cmp(a));
        let tags = match self.api_get_json(&format!("/Items/Filters?{scope}")) {
            Ok(v) => {
                let mut tags = strings(&v, "Tags");
                tags.sort_by_key(|t| t.to_lowercase());
                tags
            }
            Err(Error::NotFound(_)) => list("Tags")?,
            Err(e) => return Err(e),
        };
        Ok(MbFacets {
            genres: list("Genres")?,
            years,
            studios: list("Studios")?,
            tags,
        })
    }

    /// One item with everything a details page shows.
    pub fn item(&self, id: &str) -> Result<MbItem> {
        let user = self.user()?;
        let path = format!(
            "/Users/{user}/Items/{}?Fields={DETAIL_FIELDS}",
            encode_form_value(id)
        );
        let v = self.api_get_json(&path)?;
        let mut item = parse_item(&v, &self.base_url)
            .ok_or_else(|| Error::NotFound(format!("item {id} on {}", self.name)))?;
        item.version = self
            .chosen_version(id)
            .filter(|c| item.versions.iter().any(|v| &v.id == c));
        self.stable_item(&mut item);
        Ok(item)
    }

    /// The titles matching `text`, by section (`SEARCH_SECTIONS`), best
    /// matches first and at most `limit` a section. Sections without a
    /// match are left out.
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<(&'static str, Vec<MbItem>)>> {
        let user = self.user()?;
        let mut found = Vec::new();
        for (section, types) in SEARCH_SECTIONS {
            let v = self.api_get_json(&search_path(&user, text, types, limit))?;
            let items = self.stable_items(parse_list(&v, &self.base_url));
            if !items.is_empty() {
                found.push((section, items));
            }
        }
        // The titles still show when this fails.
        if let Ok(v) = self.api_get_json(&people_search_path(&user, text, limit)) {
            let people = self.stable_items(parse_list(&v, &self.base_url));
            if !people.is_empty() {
                found.push(("People", people));
            }
        }
        // The server searches titles only: what carries a tag with the
        // words in it is a section of its own.
        let tags = matching_tags(&self.all_tags().unwrap_or_default(), text);
        if !tags.is_empty() {
            let query = MbQuery {
                tags,
                limit,
                ..MbQuery::tagged("")
            };
            let items = self.items(&query)?.items;
            if !items.is_empty() {
                found.push(("Tagged", items));
            }
        }
        Ok(found)
    }

    /// Every tag on the server's videos, read again after a few minutes.
    fn all_tags(&self) -> Result<Vec<String>> {
        if let Some((at, tags)) = lock(&self.tags).as_ref()
            && at.elapsed() < std::time::Duration::from_secs(300)
        {
            return Ok(tags.clone());
        }
        let user = self.user()?;
        let query = format!(
            "UserId={user}&Recursive=true&IncludeItemTypes={}",
            TAGGED.join(",")
        );
        // Jellyfin lists them among its filters; Emby has no such call, but
        // a list of its own.
        let tags = match self.api_get_json(&format!("/Items/Filters?{query}")) {
            Ok(v) => strings(&v, "Tags"),
            Err(Error::NotFound(_)) => {
                let v = self.api_get_json(&format!("/Tags?{query}"))?;
                array(&v, "Items")
                    .iter()
                    .filter_map(|t| text(t, "Name"))
                    .collect()
            }
            Err(e) => return Err(e),
        };
        *lock(&self.tags) = Some((std::time::Instant::now(), tags.clone()));
        Ok(tags)
    }

    /// Partly watched films and episodes, most recent first.
    pub fn resume(&self, limit: usize) -> Result<Vec<MbItem>> {
        let user = self.user()?;
        let v = self.api_get_json(&format!(
            "/Users/{user}/Items/Resume?Limit={limit}&MediaTypes=Video&Fields={LIST_FIELDS}&{IMAGES}"
        ))?;
        Ok(self.stable_items(parse_list(&v, &self.base_url)))
    }

    /// The next episode to watch of each series (of one series with
    /// `series`).
    pub fn next_up(&self, series: Option<&str>, limit: usize) -> Result<Vec<MbItem>> {
        let user = self.user()?;
        let mut path =
            format!("/Shows/NextUp?UserId={user}&Limit={limit}&Fields={LIST_FIELDS}&{IMAGES}");
        if let Some(s) = series {
            path += &format!("&SeriesId={}", encode_form_value(s));
        }
        let v = self.api_get_json(&path)?;
        Ok(self.stable_items(parse_list(&v, &self.base_url)))
    }

    /// Newest additions to a library.
    pub fn latest(&self, library: &str, limit: usize) -> Result<Vec<MbItem>> {
        let user = self.user()?;
        let v = self.api_get_json(&format!(
            "/Users/{user}/Items/Latest?ParentId={}&Limit={limit}&Fields={LIST_FIELDS}&{IMAGES}",
            encode_form_value(library)
        ))?;
        Ok(self.stable_items(parse_list(&v, &self.base_url)))
    }

    /// The seasons of a series.
    pub fn seasons(&self, series: &str) -> Result<Vec<MbItem>> {
        let user = self.user()?;
        let v = self.api_get_json(&format!(
            "/Shows/{}/Seasons?UserId={user}&Fields={LIST_FIELDS}&{IMAGES}",
            encode_form_value(series)
        ))?;
        Ok(self.stable_items(parse_list(&v, &self.base_url)))
    }

    /// The episodes of a series, of one season with `season`.
    pub fn episodes(&self, series: &str, season: Option<&str>) -> Result<Vec<MbItem>> {
        let user = self.user()?;
        let mut path = format!(
            "/Shows/{}/Episodes?UserId={user}&Fields={LIST_FIELDS}&{IMAGES}",
            encode_form_value(series)
        );
        if let Some(s) = season {
            path += &format!("&SeasonId={}", encode_form_value(s));
        }
        let v = self.api_get_json(&path)?;
        Ok(self.stable_items(parse_list(&v, &self.base_url)))
    }

    /// The front page: libraries, then rows that are left out when the
    /// server cannot give them.
    pub fn home(&self) -> Result<MbHome> {
        let libraries = self.libraries()?;
        let soft = |r: Result<Vec<MbItem>>, what: &str| {
            r.unwrap_or_else(|e| {
                log::debug!("{what} on {}: {e}", self.name);
                Vec::new()
            })
        };
        let resume = soft(self.resume(16), "resume");
        let next_up = soft(self.next_up(None, 16), "next up");
        let latest = libraries
            .iter()
            .filter(|l| l.collection_type.as_deref() != Some("boxsets"))
            .map(|l| (l.clone(), soft(self.latest(&l.id, 16), "latest")))
            .filter(|(_, items)| !items.is_empty())
            .collect();
        Ok(MbHome {
            libraries,
            resume,
            next_up,
            latest,
        })
    }

    /// Marks or unmarks an item as a favourite on the server.
    pub fn set_favorite(&self, id: &str, favorite: bool) -> Result<()> {
        let user = self.user()?;
        let method = if favorite { "POST" } else { "DELETE" };
        let path = format!("/Users/{user}/FavoriteItems/{}", encode_form_value(id));
        self.api_request(method, &path, None)?;
        Ok(())
    }

    /// Marks an item (a whole series or season too) as watched or not.
    pub fn set_played(&self, id: &str, played: bool) -> Result<()> {
        let user = self.user()?;
        let method = if played { "POST" } else { "DELETE" };
        let path = format!("/Users/{user}/PlayedItems/{}", encode_form_value(id));
        self.api_request(method, &path, None)?;
        Ok(())
    }
}

impl crate::Catalog for MediaBrowserSource {
    fn home(&self) -> Result<MbHome> {
        Self::home(self)
    }
    fn search(&self, text: &str, limit: usize) -> Result<Vec<(&'static str, Vec<MbItem>)>> {
        Self::search(self, text, limit)
    }
    fn items(&self, query: &MbQuery) -> Result<MbPage> {
        Self::items(self, query)
    }
    fn shuffled(&self, query: &MbQuery) -> Result<Vec<String>> {
        Self::shuffled(self, query)
    }
    fn facets(&self, query: &MbQuery) -> Result<MbFacets> {
        Self::facets(self, query)
    }
    fn item(&self, id: &str) -> Result<MbItem> {
        Self::item(self, id)
    }
    fn seasons(&self, series: &str) -> Result<Vec<MbItem>> {
        Self::seasons(self, series)
    }
    fn next_up(&self, series: Option<&str>, limit: usize) -> Result<Vec<MbItem>> {
        Self::next_up(self, series, limit)
    }
    fn episodes(&self, series: &str, season: Option<&str>) -> Result<Vec<MbItem>> {
        Self::episodes(self, series, season)
    }
    fn set_favorite(&self, id: &str, favorite: bool) -> Result<()> {
        Self::set_favorite(self, id, favorite)
    }
    fn set_played(&self, id: &str, played: bool) -> Result<()> {
        Self::set_played(self, id, played)
    }
    fn set_version(&self, location: &str, version: Option<&str>) {
        Self::set_version(self, location, version);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::format::{Projection, StereoLayout};
    use fp_core::source::EntryKind;
    use serde_json::json;

    const BASE: &str = "http://jf:8096";

    #[test]
    fn versions_are_told_apart() {
        let v = json!({
            "Id": "m1",
            "Name": "Film",
            "Type": "Movie",
            "MediaSources": [
                {
                    "Id": "a",
                    "Name": "2160p",
                    "Size": 18_200_000_000_u64,
                    "MediaStreams": [
                        {"Type": "Video", "Codec": "hevc", "Height": 2160, "VideoRange": "HDR"}
                    ]
                },
                {
                    "Id": "b",
                    "Name": "Director's Cut",
                    "MediaStreams": [
                        {"Type": "Video", "Codec": "h264", "Height": 1080, "VideoRange": "SDR"}
                    ]
                }
            ]
        });
        let item = parse_item(&v, "http://s").unwrap();
        let labels: Vec<String> = item.versions.iter().map(MbVersion::label).collect();
        assert_eq!(
            labels,
            [
                "2160p · HEVC · HDR · 18.2 GB",
                "Director's Cut · 1080p · H264"
            ]
        );
        assert_eq!(item.versions[1].id, "b");
        assert_eq!(item.version, None);
    }

    #[test]
    fn movies_with_user_data_and_images() {
        let v = json!({
            "Id": "m1", "Name": "Blade Runner", "Type": "Movie",
            "ProductionYear": 1982, "RunTimeTicks": 70_200_000_000_i64,
            "CommunityRating": 7.9, "OfficialRating": "R",
            "Genres": ["Sci-Fi", "Noir"],
            "Overview": "A blade runner must pursue replicants.",
            "Path": "/media/Blade Runner (1982)_180_LR.mkv",
            "DateCreated": "2024-01-15T12:00:00.0000000Z",
            "ImageTags": {"Primary": "p1", "Thumb": "t1"},
            "BackdropImageTags": ["b1"],
            "UserData": {"PlaybackPositionTicks": 35_100_000_000_i64, "Played": false,
                         "IsFavorite": true}
        });
        let m = parse_item(&v, BASE).unwrap();
        assert_eq!(m.kind, MbKind::Movie);
        assert_eq!((m.year, m.runtime), (Some(1982), Some(7020.0)));
        assert_eq!(m.community_rating, Some(7.9));
        assert_eq!(m.genres, ["Sci-Fi", "Noir"]);
        assert!(m.favorite && !m.played);
        assert_eq!(m.position, 3510.0);
        assert_eq!(m.progress(), Some(0.5));
        assert_eq!(
            m.poster.as_deref(),
            Some("http://jf:8096/Items/m1/Images/Primary?maxHeight=480&quality=90&tag=p1")
        );
        assert_eq!(
            m.landscape.as_deref(),
            Some("http://jf:8096/Items/m1/Images/Thumb?maxWidth=640&quality=90&tag=t1")
        );
        assert_eq!(
            m.backdrop.as_deref(),
            Some("http://jf:8096/Items/m1/Images/Backdrop/0?maxWidth=1280&quality=90&tag=b1")
        );
        assert_eq!(m.location, "mb:http://jf:8096#item=m1");
        let f = m.format.unwrap();
        assert_eq!(
            (f.projection, f.stereo),
            (Projection::EQUIRECT_180, StereoLayout::SideBySide)
        );
        assert!(m.added.is_some());
        let e = m.to_entry();
        assert_eq!(e.kind, EntryKind::Video);
        assert_eq!(e.duration, Some(7020.0));
        assert_eq!(e.thumbnail_url, m.landscape);
    }

    #[test]
    fn episodes_fall_back_to_their_series() {
        let still = json!({
            "Id": "e1", "Name": "The Original", "Type": "Episode",
            "SeriesId": "s1", "SeriesName": "Westworld", "SeasonId": "se1",
            "IndexNumber": 1, "ParentIndexNumber": 1,
            "ImageTags": {"Primary": "still"},
            "ParentBackdropItemId": "s1", "ParentBackdropImageTags": ["sb"],
            "UserData": {"Played": true}
        });
        let e = parse_item(&still, BASE).unwrap();
        assert_eq!(e.episode_label().as_deref(), Some("S1:E1"));
        assert_eq!(e.full_title(), "Westworld S1:E1 · The Original");
        assert!(e.played);
        assert_eq!(e.landscape, e.poster);
        assert!(
            e.poster
                .as_ref()
                .unwrap()
                .contains("/Items/e1/Images/Primary")
        );
        assert!(
            e.backdrop
                .as_ref()
                .unwrap()
                .contains("/Items/s1/Images/Backdrop/0")
        );

        let bare = json!({
            "Id": "e2", "Name": "Chestnut", "Type": "Episode", "IndexNumber": 2,
            "SeriesId": "s1", "SeriesPrimaryImageTag": "sp", "ImageTags": {}
        });
        let e = parse_item(&bare, BASE).unwrap();
        assert!(
            e.poster
                .as_ref()
                .unwrap()
                .contains("/Items/s1/Images/Primary")
        );
        assert_eq!(e.episode_label().as_deref(), Some("E2"));
    }

    #[test]
    fn folders_series_and_people() {
        let v = json!({
            "Id": "s1", "Name": "Westworld", "Type": "Series", "ChildCount": 4,
            "UserData": {"UnplayedItemCount": 12},
            "Studios": [{"Name": "HBO"}],
            "People": [
                {"Name": "Jonathan Nolan", "Type": "Director"},
                {"Name": "Evan Rachel Wood", "Role": "Dolores", "Type": "Actor"},
                {"Name": "Ed Harris", "Role": "Man in Black", "Type": "Actor"}
            ]
        });
        let s = parse_item(&v, BASE).unwrap();
        assert_eq!(s.kind, MbKind::Series);
        assert!(!s.kind.playable() && s.kind.portrait());
        assert_eq!(s.location, "mb:http://jf:8096#folder=s1");
        assert_eq!((s.child_count, s.unplayed), (Some(4), Some(12)));
        assert_eq!(s.studios, ["HBO"]);
        let names: Vec<&str> = s.people.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(names, ["Evan Rachel Wood", "Ed Harris", "Jonathan Nolan"]);
        assert_eq!(s.people[0].1, "Dolores");
        assert_eq!(s.people[2].1, "Director");
        assert!(s.format.is_none());
        assert_eq!(s.to_entry().kind, EntryKind::Directory);
        // No image tags at all: the primary image is still offered.
        let plain = parse_item(&json!({"Id": "x", "Type": "Video"}), BASE).unwrap();
        assert!(plain.poster.is_some() && plain.landscape.is_some());
    }

    #[test]
    fn libraries_without_video_are_left_out() {
        let v = json!({"Items": [
            {"Id": "a", "Name": "Movies", "Type": "CollectionFolder", "CollectionType": "movies"},
            {"Id": "b", "Name": "Music", "Type": "CollectionFolder", "CollectionType": "music"},
            {"Id": "c", "Name": "VR", "Type": "CollectionFolder"},
            {"Id": "d", "Name": "Shows", "Type": "UserView", "CollectionType": "tvshows"}
        ]});
        let libs = video_libraries(parse_list(&v, BASE));
        let names: Vec<&str> = libs.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Movies", "VR", "Shows"]);
        assert!(libs.iter().all(|l| l.kind == MbKind::Library));
        assert_eq!(MbQuery::children(&libs[0]).types, [MbKind::Movie]);
        assert_eq!(MbQuery::children(&libs[2]).types, [MbKind::Series]);
        assert_eq!(MbQuery::children(&libs[1]).types.len(), 4);
        assert!(MbQuery::children(&libs[0]).recursive);
    }

    #[test]
    fn listing_paths() {
        let q = MbQuery {
            parent: Some("lib 1".into()),
            types: vec![MbKind::Movie],
            recursive: true,
            search: " blade runner ".into(),
            sort: MbSort::Added,
            descending: true,
            filter: MbFilter::Unplayed,
            start: 60,
            limit: 60,
            ..MbQuery::default()
        };
        let p = q.path("u1");
        assert!(p.starts_with("/Users/u1/Items?Recursive=true&SortBy=DateCreated,SortName"));
        for part in [
            "SortOrder=Descending",
            "StartIndex=60",
            "Limit=60",
            "ParentId=lib%201",
            "IncludeItemTypes=Movie",
            "Filters=IsUnplayed",
            "SearchTerm=blade%20runner",
            "EnableTotalRecordCount=true",
        ] {
            assert!(p.contains(part), "{part} missing from {p}");
        }
        let all = MbQuery::default().path("u1");
        assert!(!all.contains("ParentId") && !all.contains("Filters") && !all.contains("Search"));
    }

    #[test]
    fn filters_go_to_the_server() {
        let q = MbQuery {
            parent: Some("lib".into()),
            genres: vec!["Sci-Fi".into(), "Noir".into()],
            years: vec![1990, 1991],
            studios: vec!["A & B".into()],
            min_width: Some(7680),
            sort: MbSort::LastWatched,
            ..MbQuery::default()
        };
        let p = q.path("u1");
        for part in [
            "SortBy=DatePlayed,SortName",
            "&Genres=Sci-Fi%7CNoir",
            "&Years=1990,1991",
            "&Studios=A%20%26%20B",
            "&MinWidth=7680",
        ] {
            assert!(p.contains(part), "{part} missing from {p}");
        }
        assert!(MbSort::LastWatched.default_descending());
        // A shuffle asks once for every id, with the same filters and no
        // paging, then for its pages by id.
        let s = q.shuffled_path("u1");
        assert!(
            s.contains("SortBy=Random") && s.contains("&Genres=Sci-Fi%7CNoir"),
            "{s}"
        );
        assert!(!s.contains("StartIndex") && !s.contains("Limit="), "{s}");
        let page = MbQuery {
            ids: vec!["b".into(), "a".into()],
            ..q.clone()
        };
        assert!(page.path("u1").contains("&Ids=b,a"));
    }

    #[test]
    fn people_are_found_and_open_what_they_are_in() {
        let p = people_search_path("u1", " ana rivera ", 20);
        assert!(
            p.starts_with("/Persons?UserId=u1&SearchTerm=ana%20rivera&Limit=20&"),
            "{p}"
        );
        let found = parse_list(
            &json!({"Items": [{"Id": "p 1", "Name": "Ana Rivera", "Type": "Person"}]}),
            "http://h",
        );
        assert_eq!(found[0].kind, MbKind::Person);
        let path = MbQuery::children(&found[0]).path("u1");
        assert!(path.contains("&PersonIds=p%201"), "{path}");
        assert!(
            path.contains("Recursive=true") && !path.contains("ParentId"),
            "{path}"
        );
    }

    #[test]
    fn a_shuffle_keeps_its_order_and_facets_are_names() {
        let list = parse_list(
            &json!({"Items": [
                {"Id": "a", "Name": "A", "Type": "Movie"},
                {"Id": "c", "Name": "C", "Type": "Movie"},
                {"Id": "b", "Name": "B", "Type": "Movie"}
            ]}),
            BASE,
        );
        let order: Vec<String> = ["b", "a", "x"].iter().map(ToString::to_string).collect();
        let got: Vec<String> = in_order(list, &order).into_iter().map(|i| i.id).collect();
        assert_eq!(got, ["b", "a"], "in the shuffle's order, others left out");
        assert_eq!(
            ids(&json!({"Items": [{"Id": "z"}, {"Id": "y"}]})),
            ["z", "y"]
        );
        let v = json!({"Items": [{"Name": "drama"}, {"Name": " Comedy "}, {"Name": "Drama"}, {}]});
        assert_eq!(names(&v), ["Comedy", "drama", "Drama"]);
        let scope = MbQuery::default().scope_params("u1");
        assert!(scope.contains("IncludeItemTypes=Movie,Series,Episode,Video,MusicVideo"));
    }

    #[test]
    fn tags_are_searched_and_listed() {
        let tags: Vec<String> = ["Beach Day", "beach", "Sunset", "Night Beach Party"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            matching_tags(&tags, "BEACH"),
            ["Beach Day", "beach", "Night Beach Party"]
        );
        assert_eq!(matching_tags(&tags, "beach party"), ["Night Beach Party"]);
        assert!(matching_tags(&tags, "  ").is_empty());
        let p = MbQuery::tagged("Beach Day").path("u1");
        assert!(p.contains("&Tags=Beach%20Day"), "{p}");
        assert!(p.contains("Recursive=true"), "{p}");
        assert!(
            p.contains("IncludeItemTypes=Movie,Series,Episode,Video,MusicVideo"),
            "{p}"
        );
        let two = MbQuery {
            tags: vec!["a".into(), "b".into()],
            ..MbQuery::default()
        };
        assert!(two.path("u1").contains("&Tags=a%7Cb"));
    }

    #[test]
    fn search_looks_for_titles_ranked_by_the_server() {
        let p = search_path("u1", " blade runner ", &["Video", "MusicVideo"], 50);
        assert!(
            p.starts_with("/Users/u1/Items?SearchTerm=blade%20runner&"),
            "{p}"
        );
        for part in [
            "IncludeItemTypes=Video,MusicVideo",
            "Recursive=true",
            "Limit=50",
        ] {
            assert!(p.contains(part), "{part} missing from {p}");
        }
        assert!(!p.contains("SortBy"), "the server ranks the matches: {p}");
        let types: Vec<&str> = SEARCH_SECTIONS
            .iter()
            .flat_map(|(_, t)| t.iter().copied())
            .collect();
        assert!(types.contains(&"Movie") && types.contains(&"Series"));
        assert!(!types.contains(&"Episode"));
    }

    #[test]
    fn pages_and_bare_arrays() {
        let page = parse_page(
            &json!({"Items": [{"Id": "1", "Type": "Movie"}], "TotalRecordCount": 250}),
            BASE,
        );
        assert_eq!((page.items.len(), page.total), (1, 250));
        let latest = parse_list(
            &json!([{"Id": "1", "Type": "Movie"}, {"Name": "no id"}]),
            BASE,
        );
        assert_eq!(latest.len(), 1);
    }
}
