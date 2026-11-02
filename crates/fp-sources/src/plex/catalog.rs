//! Maps Plex library and metadata responses to the shared catalog, keeping
//! Plex's item types, query keys and pagination inside this adapter.

use super::{BACKDROP, LANDSCAPE, LISTING_LIMIT, PAGE_LIMIT, POSTER, PlexSource, container};
use crate::catalog::{
    Catalog, CatalogCapabilities, CatalogFacets, CatalogFilter, CatalogHome, CatalogItem,
    CatalogKind, CatalogPage, CatalogQuery, CatalogSort, CatalogVersion,
};
use crate::error::{Error, Result};
use crate::json::{array, boolean, num, text, uint};
use crate::urlutil::encode_form_value;
use fp_core::format::detect_from_name;
use fp_core::sync::lock;
use serde_json::Value;
use std::collections::HashSet;

/// The videos in progress and next up, directly: `/hubs/continueWatching`
/// without `/items` wraps them in a hub.
const CONTINUE_WATCHING: &str = "/hubs/continueWatching/items";

impl PlexSource {
    pub(super) fn libraries(&self) -> Result<Vec<CatalogItem>> {
        self.client()?;
        // Sections is available to shared users too; section/all is the
        // main provider's newer endpoint and is not universal on older PMS.
        let v = match self.json("/library/sections") {
            Ok(v) => v,
            Err(Error::NotFound(_)) => self.json("/library/sections/all")?,
            Err(e) => return Err(e),
        };
        Ok(array(container(&v), "Directory")
            .iter()
            .filter_map(|section| {
                let typ = text(section, "type")?;
                if !matches!(typ.as_str(), "movie" | "show" | "video") {
                    return None;
                }
                let id = text(section, "key")?;
                let item = CatalogItem {
                    id: format!("section:{id}"),
                    name: text(section, "title").unwrap_or_default(),
                    kind: CatalogKind::Library,
                    collection_type: Some(
                        match typ.as_str() {
                            "movie" => "movies",
                            "show" => "tvshows",
                            _ => "homevideos",
                        }
                        .into(),
                    ),
                    location: self.location("section", &id, None),
                    poster: text(section, "thumb").and_then(|p| self.image(&p, POSTER)),
                    backdrop: text(section, "art").and_then(|p| self.image(&p, BACKDROP)),
                    ..Default::default()
                };
                Some(item)
            })
            .collect())
    }

    pub(super) fn catalog_item(&self, v: &Value) -> Option<CatalogItem> {
        let id = text(v, "ratingKey")?;
        let kind = match text(v, "type")?.as_str() {
            "movie" if text(v, "subtype").as_deref() == Some("clip") => CatalogKind::Video,
            "movie" => CatalogKind::Movie,
            "show" => CatalogKind::Series,
            "season" => CatalogKind::Season,
            "episode" => CatalogKind::Episode,
            "clip" | "video" => CatalogKind::Video,
            "collection" => CatalogKind::BoxSet,
            "folder" => CatalogKind::Folder,
            _ => return None,
        };
        let versions: Vec<CatalogVersion> = array(v, "Media")
            .iter()
            .filter_map(|media| {
                let id = text(media, "id")?;
                let path = array(media, "Part").first().and_then(|p| text(p, "file"));
                let video = array(media, "Part")
                    .iter()
                    .flat_map(|p| array(p, "Stream"))
                    .find(|s| num(s, "streamType") == Some(1.0));
                let range = video.and_then(|s| {
                    if boolean(s, "DOVIPresent") == Some(true) {
                        Some("Dolby Vision".into())
                    } else if uint(s, "bitDepth").is_some_and(|n| n > 8)
                        && text(s, "colorTrc")
                            .is_some_and(|s| s.contains("smpte2084") || s.contains("arib"))
                    {
                        Some("HDR".into())
                    } else {
                        None
                    }
                });
                Some(CatalogVersion {
                    id,
                    name: text(media, "videoResolution")
                        .map(|s| {
                            if s.chars().all(|c| c.is_ascii_digit()) {
                                format!("{s}p")
                            } else {
                                s
                            }
                        })
                        .unwrap_or_default(),
                    height: uint(media, "height"),
                    codec: text(media, "videoCodec")
                        .or_else(|| video.and_then(|s| text(s, "codec"))),
                    range,
                    size: array(media, "Part").iter().try_fold(0u64, |sum, p| {
                        num(p, "size").map(|n| sum.saturating_add(n.max(0.0) as u64))
                    }),
                    format: path.as_deref().and_then(detect_from_name),
                })
            })
            .collect();
        let selected = lock(&self.versions).get(&id).cloned();
        let selected_media = self.choose_media(v, selected.as_deref()).ok();
        let format = selected_media
            .and_then(|m| array(m, "Part").first())
            .and_then(|p| text(p, "file"))
            .as_deref()
            .and_then(detect_from_name)
            .or_else(|| text(v, "title").as_deref().and_then(detect_from_name));
        let thumb = text(v, "thumb")
            .or_else(|| text(v, "parentThumb"))
            .or_else(|| text(v, "grandparentThumb"));
        let art = text(v, "art").or_else(|| text(v, "grandparentArt"));
        // An episode's thumb is its still, wider than anything's poster.
        let wide = if kind == CatalogKind::Episode {
            thumb.as_ref().or(art.as_ref())
        } else {
            art.as_ref().or(thumb.as_ref())
        };
        let landscape = wide.and_then(|p| self.image(p, LANDSCAPE));
        let poster = thumb.and_then(|p| self.image(&p, POSTER));
        let backdrop = art.and_then(|p| self.image(&p, BACKDROP));
        let unplayed = num(v, "leafCount")
            .zip(num(v, "viewedLeafCount"))
            .map(|(n, v)| (n - v).max(0.0) as u32);
        // A show's or season's viewCount counts plays and stays up when
        // episodes are marked unwatched: only its episodes say it is finished.
        let viewed = match kind {
            CatalogKind::Series | CatalogKind::Season => {
                unplayed == Some(0) && num(v, "leafCount").is_some_and(|n| n > 0.0)
            }
            _ => num(v, "viewCount").unwrap_or(0.0) > 0.0,
        };
        Some(CatalogItem {
            id: id.clone(),
            name: text(v, "title").unwrap_or_default(),
            kind,
            overview: text(v, "summary"),
            year: num(v, "year").map(|n| n as i32),
            runtime: num(v, "duration").map(|n| n / 1000.0),
            community_rating: num(v, "audienceRating")
                .or_else(|| num(v, "rating"))
                .map(|n| n as f32),
            official_rating: text(v, "contentRating"),
            genres: tags(v, "Genre"),
            tags: Vec::new(),
            studios: text(v, "studio").into_iter().collect(),
            people: array(v, "Role")
                .iter()
                .filter_map(|p| Some((text(p, "tag")?, text(p, "role").unwrap_or_default())))
                .collect(),
            series_id: text(v, "grandparentRatingKey").or_else(|| {
                (kind == CatalogKind::Season)
                    .then(|| text(v, "parentRatingKey"))
                    .flatten()
            }),
            series_name: text(v, "grandparentTitle").or_else(|| {
                (kind == CatalogKind::Season)
                    .then(|| text(v, "parentTitle"))
                    .flatten()
            }),
            season_id: (kind == CatalogKind::Episode)
                .then(|| text(v, "parentRatingKey"))
                .flatten(),
            index: num(v, "index").map(|n| n.max(0.0) as u32),
            parent_index: num(v, "parentIndex").map(|n| n.max(0.0) as u32),
            child_count: num(v, "childCount")
                .or_else(|| num(v, "leafCount"))
                .map(|n| n.max(0.0) as u32),
            played: viewed,
            position: num(v, "viewOffset").map_or(0.0, |n| n / 1000.0),
            unplayed,
            added: num(v, "addedAt").map(|n| n as i64),
            landscape,
            poster,
            backdrop,
            location: self.location("item", &id, None),
            format,
            versions,
            version: selected,
            ..Default::default()
        })
    }

    pub(super) fn choose_media<'a>(
        &self,
        item: &'a Value,
        selected: Option<&str>,
    ) -> Result<&'a Value> {
        let media = array(item, "Media");
        if let Some(id) = selected {
            if let Some(chosen) = media.iter().find(|m| text(m, "id").as_deref() == Some(id)) {
                return Ok(chosen);
            }
            // Its file was replaced on the server. The video is still there.
            log::info!("Plex version {id} is gone; choosing another");
        }
        let max = lock(&self.config).max_height;
        media
            .iter()
            .filter(|m| max.is_none_or(|max| uint(m, "height").is_none_or(|h| h <= max)))
            .max_by_key(|m| uint(m, "height").unwrap_or(0))
            .or_else(|| {
                media
                    .iter()
                    .min_by_key(|m| uint(m, "height").unwrap_or(u32::MAX))
            })
            .ok_or_else(|| Error::Unsupported("Plex item has no playable media version".into()))
    }

    /// One request's page, and how many items the server sent: more than
    /// the page has when some aren't kept.
    fn page(&self, path: &str, query: &CatalogQuery) -> Result<(CatalogPage, usize)> {
        let v = self.json(path)?;
        let c = container(&v);
        let raw = array(c, "Metadata");
        let mut items: Vec<_> = raw.iter().filter_map(|v| self.catalog_item(v)).collect();
        if !query.types.is_empty() {
            items.retain(|i| {
                query.types.contains(&i.kind)
                    || (matches!(i.kind, CatalogKind::Movie | CatalogKind::Video)
                        && query
                            .types
                            .iter()
                            .any(|k| matches!(k, CatalogKind::Movie | CatalogKind::Video)))
            });
        }
        let offset = num(c, "offset").map_or(query.start, |n| n.max(0.0) as usize);
        if offset != query.start && !raw.is_empty() {
            return Err(Error::parse(
                "Plex page",
                "server returned a different page offset",
            ));
        }
        let total = num(c, "totalSize")
            .map(|n| n.max(0.0) as usize)
            .unwrap_or_else(|| offset.saturating_add(raw.len()));
        Ok((CatalogPage { items, total }, raw.len()))
    }

    /// Up to `query.limit` items from `query.start`, over as many requests as
    /// Plex's page limit takes. A grid reloading all it shows can ask for more
    /// than one page holds.
    fn listing(&self, query: &CatalogQuery) -> Result<CatalogPage> {
        let wanted = query.limit.max(1);
        let mut q = query.clone();
        let mut sent = 0;
        let mut items = Vec::new();
        let mut last_first = None;
        loop {
            q.limit = (wanted - sent).min(PAGE_LIMIT);
            let (page, raw) = self.page(&self.query_path(&q)?, &q)?;
            if wanted >= LISTING_LIMIT && page.total > LISTING_LIMIT {
                return Err(Error::Unsupported("Plex listing is too large".into()));
            }
            let first = page.items.first().map(|i| i.id.clone());
            if first.is_some() && first == last_first {
                return Err(Error::parse(
                    "Plex listing",
                    "server listing did not advance",
                ));
            }
            last_first = first;
            items.extend(page.items);
            sent += raw;
            q.start += raw;
            if raw == 0 || sent >= wanted || q.start >= page.total {
                return Ok(CatalogPage {
                    items,
                    total: page.total,
                });
            }
        }
    }

    pub(super) fn whole_listing(&self, query: CatalogQuery) -> Result<Vec<CatalogItem>> {
        let page = self.items(&CatalogQuery {
            start: 0,
            limit: LISTING_LIMIT,
            ..query
        })?;
        Ok(page.items)
    }

    fn query_path(&self, query: &CatalogQuery) -> Result<String> {
        if query.filter == CatalogFilter::Favorites
            || query.min_width.is_some()
            || query.person.is_some()
            || !query.tags.is_empty()
        {
            return Err(Error::Unsupported(
                "this Plex catalog filter is not available".into(),
            ));
        }
        let (path, section) = match query.parent.as_deref() {
            Some(p) if p.starts_with("section:") => {
                let section = p.trim_start_matches("section:");
                (
                    format!("/library/sections/{}/all", encode_form_value(section)),
                    Some(section),
                )
            }
            Some(p) => (
                format!("/library/metadata/{}/children", encode_form_value(p)),
                None,
            ),
            None => ("/library/all".to_string(), None),
        };
        let mut url =
            url::Url::parse(&format!("http://placeholder{path}")).expect("an encoded path");
        {
            let mut p = url.query_pairs_mut();
            p.append_pair("X-Plex-Container-Start", &query.start.to_string());
            p.append_pair(
                "X-Plex-Container-Size",
                &query.limit.clamp(1, PAGE_LIMIT).to_string(),
            );
            let key = match query.sort {
                CatalogSort::Name => "titleSort",
                CatalogSort::Added => "addedAt",
                CatalogSort::Released => "originallyAvailableAt",
                CatalogSort::Rating => "audienceRating",
                CatalogSort::Runtime => "duration",
                CatalogSort::LastWatched => "lastViewedAt",
                CatalogSort::Random => "random",
            };
            p.append_pair(
                "sort",
                &format!("{key}:{}", if query.descending { "desc" } else { "asc" }),
            );
            if let Some(kind) = query.types.first().and_then(|k| type_number(*k)) {
                p.append_pair("type", kind);
            }
            if !query.search.trim().is_empty() {
                p.append_pair("title", query.search.trim());
            }
            match query.filter {
                CatalogFilter::Unplayed => {
                    p.append_pair("unwatched", "1");
                }
                CatalogFilter::Resumable => {
                    p.append_pair("inProgress", "1");
                }
                _ => {}
            }
            for (key, values) in [("genre", &query.genres), ("studio", &query.studios)] {
                if !values.is_empty() {
                    let section = section.ok_or_else(|| {
                        Error::Unsupported("Plex facets require a library".into())
                    })?;
                    let values: Vec<_> = values
                        .iter()
                        .map(|name| {
                            if key == "studio" {
                                // Studios are strings, not genre IDs. PMS's
                                // choice key may already be URL-encoded. It
                                // splits values at commas, then decodes each,
                                // so a comma in a name is escaped once more.
                                name.replace('%', "%25").replace(',', "%2C")
                            } else {
                                lock(&self.facets)
                                    .get(&format!("{section}/{key}/{name}"))
                                    .cloned()
                                    .unwrap_or_else(|| name.clone())
                            }
                        })
                        .collect();
                    // String equality is `studio==...`; plain `studio=...`
                    // performs a substring match instead.
                    p.append_pair(
                        if key == "studio" { "studio=" } else { key },
                        &values.join(","),
                    );
                }
            }
            if !query.years.is_empty() {
                p.append_pair(
                    "year",
                    &query
                        .years
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
        }
        Ok(format!(
            "{}?{}",
            url.path(),
            url.query().unwrap_or_default()
        ))
    }

    fn facet(&self, section: &str, kind: &str) -> Result<Vec<String>> {
        let v = self.json(&format!(
            "/library/sections/{}/{kind}",
            encode_form_value(section)
        ))?;
        let mut names = Vec::new();
        for d in array(container(&v), "Directory") {
            if let (Some(name), Some(key)) = (text(d, "title"), text(d, "key")) {
                lock(&self.facets).insert(format!("{section}/{kind}/{name}"), key);
                names.push(name);
            }
        }
        Ok(names)
    }
}

impl Catalog for PlexSource {
    fn capabilities(&self) -> CatalogCapabilities {
        CatalogCapabilities {
            favorites: false,
            width_filter: false,
            people: false,
            tags: false,
        }
    }

    fn home(&self) -> Result<CatalogHome> {
        let libraries = self.libraries()?;
        let resume = self
            .json(&format!("{CONTINUE_WATCHING}?X-Plex-Container-Size=20"))
            .map(|v| {
                array(container(&v), "Metadata")
                    .iter()
                    .filter_map(|v| self.catalog_item(v))
                    .collect()
            })
            .unwrap_or_default();
        let mut latest = Vec::new();
        for library in &libraries {
            let page = self.items(&CatalogQuery {
                parent: Some(library.id.clone()),
                sort: CatalogSort::Added,
                descending: true,
                limit: 12,
                ..Default::default()
            });
            match page {
                Ok(page) if !page.items.is_empty() => latest.push((library.clone(), page.items)),
                Ok(_) => {}
                Err(e) => log::debug!("latest in {} on {}: {e}", library.name, self.name),
            }
        }
        Ok(CatalogHome {
            libraries,
            resume,
            next_up: Vec::new(),
            latest,
        })
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<(&'static str, Vec<CatalogItem>)>> {
        let v = self.json(&format!(
            "/hubs/search?query={}&limit={}",
            encode_form_value(query),
            limit.clamp(1, PAGE_LIMIT)
        ))?;
        let mut groups: Vec<(&'static str, Vec<CatalogItem>)> = Vec::new();
        for hub in array(container(&v), "Hub") {
            let typ = text(hub, "type").unwrap_or_default();
            let title = match typ.as_str() {
                "movie" => "Movies",
                "show" => "Shows",
                "collection" => "Collections",
                "episode" | "clip" => "Videos",
                _ => continue,
            };
            let found: Vec<_> = array(hub, "Metadata")
                .iter()
                .filter_map(|v| self.catalog_item(v))
                .collect();
            if found.is_empty() {
                continue;
            }
            if let Some((_, items)) = groups.iter_mut().find(|(k, _)| *k == title) {
                items.extend(found);
            } else {
                groups.push((title, found));
            }
        }
        Ok(groups)
    }

    fn items(&self, query: &CatalogQuery) -> Result<CatalogPage> {
        if !query.ids.is_empty() {
            let mut items = Vec::new();
            for chunk in query.ids.chunks(60) {
                let ids = chunk
                    .iter()
                    .map(|id| encode_form_value(id))
                    .collect::<Vec<_>>()
                    .join(",");
                match self.json(&format!("/library/metadata/{ids}")) {
                    Ok(value) => {
                        let mut found: std::collections::HashMap<_, _> =
                            array(container(&value), "Metadata")
                                .iter()
                                .filter_map(|v| self.catalog_item(v).map(|i| (i.id.clone(), i)))
                                .collect();
                        items.extend(chunk.iter().filter_map(|id| found.remove(id)));
                    }
                    Err(Error::NotFound(_)) => {
                        for id in chunk {
                            match self.item(id) {
                                Ok(item) => items.push(item),
                                Err(Error::NotFound(_)) => {}
                                Err(e) => return Err(e),
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            return Ok(CatalogPage {
                total: items.len(),
                items,
            });
        }
        self.listing(query)
    }

    fn shuffled(&self, query: &CatalogQuery) -> Result<Vec<String>> {
        let listed = self.whole_listing(CatalogQuery {
            sort: CatalogSort::Name,
            ..query.clone()
        })?;
        let mut seen = HashSet::new();
        let mut ids: Vec<String> = listed
            .into_iter()
            .map(|item| item.id)
            .filter(|id| seen.insert(id.clone()))
            .collect();
        // PMS random pagination is not stable. Snapshot by a stable sort,
        // then assign each id a local seeded key exactly once.
        let seed = fp_core::playback::now_ms() as u64;
        ids.sort_by_cached_key(|id| fp_core::shuffle::key(seed, id));
        Ok(ids)
    }

    fn facets(&self, query: &CatalogQuery) -> Result<CatalogFacets> {
        let Some(section) = query
            .parent
            .as_deref()
            .and_then(|p| p.strip_prefix("section:"))
        else {
            return Ok(CatalogFacets::default());
        };
        let genres = self.facet(section, "genre")?;
        let studios = self.facet(section, "studio")?;
        let mut years: Vec<i32> = self
            .facet(section, "year")?
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        years.sort_unstable_by(|a, b| b.cmp(a));
        Ok(CatalogFacets {
            genres,
            studios,
            years,
            tags: Vec::new(),
        })
    }

    fn item(&self, id: &str) -> Result<CatalogItem> {
        if id.starts_with("section:") {
            return self
                .libraries()?
                .into_iter()
                .find(|i| i.id == id)
                .ok_or_else(|| Error::NotFound("Plex library".into()));
        }
        self.catalog_item(&self.raw_item(id)?)
            .ok_or_else(|| Error::Unsupported("Plex item is not a video".into()))
    }

    fn seasons(&self, series: &str) -> Result<Vec<CatalogItem>> {
        self.all_children(series, Some(CatalogKind::Season))
    }
    fn episodes(&self, series: &str, season: Option<&str>) -> Result<Vec<CatalogItem>> {
        match season {
            Some(season) => self.all_children(season, Some(CatalogKind::Episode)),
            None => {
                let mut episodes = Vec::new();
                for season in self.seasons(series)? {
                    episodes.extend(self.all_children(&season.id, Some(CatalogKind::Episode))?);
                }
                Ok(episodes)
            }
        }
    }
    fn next_up(&self, series: Option<&str>, limit: usize) -> Result<Vec<CatalogItem>> {
        let v = self.json(&format!("{CONTINUE_WATCHING}?X-Plex-Container-Size=100"))?;
        Ok(array(container(&v), "Metadata")
            .iter()
            .filter_map(|v| self.catalog_item(v))
            .filter(|i| {
                i.kind == CatalogKind::Episode
                    && series.is_none_or(|s| i.series_id.as_deref() == Some(s))
            })
            .take(limit)
            .collect())
    }
    fn set_played(&self, id: &str, played: bool) -> Result<()> {
        self.request(
            "PUT",
            &format!(
                "/:/{}?identifier=com.plexapp.plugins.library&key={}",
                if played { "scrobble" } else { "unscrobble" },
                encode_form_value(id)
            ),
            &[],
        )?;
        // Marking a show marks its episodes too.
        lock(&self.metadata).clear();
        Ok(())
    }
    fn set_version(&self, location: &str, version: Option<&str>) {
        let Ok(location) = self.parse_location(location) else {
            return;
        };
        if location.kind != "item" {
            return;
        }
        let id = location.id;
        let mut versions = lock(&self.versions);
        match version {
            Some(v) => {
                versions.insert(id.to_string(), v.to_string());
            }
            None => {
                versions.remove(&id);
            }
        }
    }
}

impl PlexSource {
    fn all_children(&self, id: &str, kind: Option<CatalogKind>) -> Result<Vec<CatalogItem>> {
        let mut items = self.whole_listing(CatalogQuery {
            parent: Some(id.into()),
            types: kind.into_iter().collect(),
            ..Default::default()
        })?;
        if matches!(kind, Some(CatalogKind::Season | CatalogKind::Episode)) {
            items.sort_by_key(|i| (i.parent_index, i.index));
        }
        Ok(items)
    }
}

fn tags(v: &Value, key: &str) -> Vec<String> {
    array(v, key)
        .iter()
        .filter_map(|t| text(t, "tag"))
        .collect()
}
fn type_number(kind: CatalogKind) -> Option<&'static str> {
    match kind {
        CatalogKind::Movie | CatalogKind::Video | CatalogKind::MusicVideo => Some("1"),
        CatalogKind::Series => Some("2"),
        CatalogKind::Season => Some("3"),
        CatalogKind::Episode => Some("4"),
        CatalogKind::BoxSet => Some("18"),
        _ => None,
    }
}
