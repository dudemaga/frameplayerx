//! A media-server catalog: the server's libraries and rows of what to watch
//! next, poster grids the server searches, sorts, shuffles and filters
//! (genres, decades, resolution, studios, tags), series with their seasons
//! and episodes, and details pages. Watched marks, resume points and
//! favourites are the server's own.

use super::page::{self, Card};
use super::theme::{self, Weight};
use super::widgets::{self, Kind, Tip};
use super::{Action, View, fmt_time, icons};
use crate::playback::OpenRequest;
use egui::{Align2, Color32, RichText, Sense, Vec2};
use fp_sources::{
    Catalog, CatalogCapabilities, CatalogFacets, CatalogFilter, CatalogHome, CatalogItem,
    CatalogKind, CatalogPage, CatalogQuery, CatalogSort,
};
use std::time::{Duration, Instant};

/// Typing rests this long before a search goes to the server.
const SEARCH_DELAY: Duration = Duration::from_millis(600);
/// Items per page of a grid.
const PAGE: usize = 60;
/// The widths the Resolution filter keeps at least.
const RESOLUTIONS: [(u32, &str); 4] = [(1280, "HD"), (3840, "4K"), (5760, "6K"), (7680, "8K")];
/// Matches shown per section of a search.
const SEARCH_LIMIT: usize = 50;
/// Height of the banners of series and details pages.
const BANNER: f32 = 320.0;

/// The pages open on a server, the one shown last.
pub struct CatalogView {
    pub pages: Vec<Page>,
}

pub struct Page {
    /// The page's identity; its facets use it independently of data loads.
    pub seq: u64,
    /// Only the latest listing/search/details request can update the page.
    pub(super) request_seq: Option<u64>,
    pub loading: bool,
    pub error: Option<String>,
    pub body: Body,
}

pub enum Body {
    /// Libraries and rows of what to watch.
    Home {
        home: Option<CatalogHome>,
        search: String,
        /// When the search text last changed.
        typed: Option<Instant>,
    },
    /// What a search of the server found, a row per kind (films, series,
    /// collections, other videos), as Jellyfin shows it.
    Search {
        /// The search field, in the header.
        search: String,
        typed: Option<Instant>,
        /// What the results are for.
        asked: String,
        results: Vec<(&'static str, Vec<CatalogItem>)>,
    },
    /// A library, collection or folder, a page at a time.
    Grid(Box<Grid>),
    Series {
        series: Box<CatalogItem>,
        seasons: Vec<CatalogItem>,
        season: Option<String>,
        episodes: Vec<CatalogItem>,
        next_up: Option<Box<CatalogItem>>,
    },
    /// A film, episode or video.
    Item { item: Box<CatalogItem> },
}

/// A library, collection or folder, a page at a time.
pub struct Grid {
    pub title: String,
    pub query: CatalogQuery,
    pub items: Vec<CatalogItem>,
    pub total: usize,
    pub typed: Option<Instant>,
    /// Every item of a shuffle, in its order, which its pages follow.
    pub order: Option<Vec<String>>,
    pub facets: Facets,
    pub filters_open: bool,
    /// How the drawer's long sections are narrowed, by title.
    pub lists: std::collections::HashMap<&'static str, page::ListState>,
    /// The tags the page is for ("Tagged ..."), which are not filters.
    pub base_tags: Vec<String>,
    /// Sorted or filtered anew: the grid goes back to its top.
    pub to_top: bool,
}

/// A grid's genres, years, studios and tags, asked for when its Filters
/// drawer first opens.
pub enum Facets {
    Unasked,
    Asked,
    Got(CatalogFacets),
    Failed(String),
}

/// One filter that is on, as its chip under the sorts names it.
enum On {
    Genre(String),
    Decade(i32),
    Width(u32),
    Studio(String),
    Tag(String),
}

impl Grid {
    fn new(title: String, query: CatalogQuery) -> Grid {
        Grid {
            title,
            base_tags: query.tags.clone(),
            query,
            items: Vec::new(),
            total: 0,
            typed: None,
            order: None,
            facets: Facets::Unasked,
            filters_open: false,
            lists: Default::default(),
            to_top: false,
        }
    }

    fn on(&self) -> Vec<On> {
        let q = &self.query;
        let mut on: Vec<On> = q.genres.iter().cloned().map(On::Genre).collect();
        on.extend(decades_on(&q.years).into_iter().map(On::Decade));
        on.extend(q.min_width.map(On::Width));
        on.extend(q.studios.iter().cloned().map(On::Studio));
        on.extend(
            q.tags
                .iter()
                .filter(|t| !self.base_tags.contains(t))
                .cloned()
                .map(On::Tag),
        );
        on
    }

    /// The chips of the filters that are on.
    fn labels(&self) -> Vec<String> {
        self.on()
            .into_iter()
            .map(|o| match o {
                On::Genre(g) => tr!("filter-on-genre", genre = g),
                On::Decade(d) => tr!("filter-decade", decade = d),
                On::Width(w) => tr!("filter-width-and-up", width = width_label(w)),
                On::Studio(s) => tr!("filter-on-studio", studio = s),
                On::Tag(t) => tr!("filter-on-tag", tag = t),
            })
            .collect()
    }

    /// Takes off the filter of chip `i` (see [`Grid::labels`]).
    fn take_off(&mut self, i: usize) {
        let on = self.on().into_iter().nth(i);
        let q = &mut self.query;
        match on {
            Some(On::Genre(g)) => q.genres.retain(|x| *x != g),
            Some(On::Decade(d)) => q.years.retain(|y| !(d..d + 10).contains(y)),
            Some(On::Width(_)) => q.min_width = None,
            Some(On::Studio(s)) => q.studios.retain(|x| *x != s),
            Some(On::Tag(t)) => q.tags.retain(|x| *x != t),
            None => {}
        }
    }

    fn clear_filters(&mut self) {
        let q = &mut self.query;
        q.genres.clear();
        q.years.clear();
        q.min_width = None;
        q.studios.clear();
        q.tags.retain(|t| self.base_tags.contains(t));
    }

    /// A changed filter/search/sort starts a new listing, including a new
    /// shuffle snapshot; old page IDs must not leak into a later refresh.
    fn restart(&mut self) -> CatalogQuery {
        self.query.start = 0;
        self.query.limit = PAGE;
        self.query.ids.clear();
        self.order = None;
        self.to_top = true;
        self.query.clone()
    }

    /// Choosing another action this frame must leave an unsent facets
    /// request eligible for the next frame.
    fn unasked_facets(&self) -> Option<CatalogQuery> {
        matches!(self.facets, Facets::Unasked).then(|| self.query.clone())
    }

    /// The next page: of a shuffle, its next items by id.
    fn next_page(&self) -> CatalogQuery {
        let start = self.items.len();
        let ids = self
            .order
            .as_ref()
            .map(|o| o.iter().skip(start).take(PAGE).cloned().collect())
            .unwrap_or_default();
        CatalogQuery {
            start,
            limit: PAGE,
            ids,
            ..self.query.clone()
        }
    }
}

/// The decades `years` make, newest first.
fn decades(years: &[i32]) -> Vec<i32> {
    let mut d: Vec<i32> = years.iter().map(|y| y - y.rem_euclid(10)).collect();
    d.sort_unstable_by(|a, b| b.cmp(a));
    d.dedup();
    d
}

/// The decades a query keeps whole (the Released filter adds and takes
/// off a decade's ten years).
fn decades_on(years: &[i32]) -> Vec<i32> {
    decades(years)
        .into_iter()
        .filter(|d| (*d..*d + 10).all(|y| years.contains(&y)))
        .collect()
}

fn width_label(w: u32) -> &'static str {
    RESOLUTIONS
        .iter()
        .find(|(x, _)| *x == w)
        .map_or("", |(_, label)| label)
}

/// What to ask a server for.
#[derive(Clone, Debug)]
pub enum Load {
    Home,
    /// The titles matching the text.
    Search(String),
    /// A page of a listing; past the first it adds to what is shown. A
    /// shuffle's first asks for its order too.
    Grid(CatalogQuery),
    /// What a listing's library can be filtered by.
    Facets(CatalogQuery),
    /// A series with its seasons and one season's episodes (`season`, else
    /// the one to watch next).
    Series {
        id: String,
        season: Option<String>,
    },
    Episodes {
        series: String,
        season: String,
    },
    Item {
        id: String,
    },
}

/// A mark the server keeps per item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Favorite,
    Played,
}

/// A server's answer to a [`Load`].
pub enum Loaded {
    Home(CatalogHome),
    Search {
        text: String,
        results: Vec<(&'static str, Vec<CatalogItem>)>,
    },
    Grid {
        page: CatalogPage,
        append: bool,
        /// A new shuffle's order.
        order: Option<Vec<String>>,
        /// Items asked for by id (a shuffle's): the order stays.
        by_id: bool,
    },
    /// Failing quietly: the drawer says so, the page goes on.
    Facets(Result<CatalogFacets, String>),
    Series {
        series: Box<CatalogItem>,
        seasons: Vec<CatalogItem>,
        season: Option<String>,
        episodes: Vec<CatalogItem>,
        next_up: Option<Box<CatalogItem>>,
    },
    Episodes(Vec<CatalogItem>),
    Item(Box<CatalogItem>),
}

/// Asks the server (blocking: run it on a job thread).
pub fn run(src: &dyn Catalog, load: &Load) -> fp_sources::Result<Loaded> {
    Ok(match load {
        Load::Home => Loaded::Home(src.home()?),
        Load::Search(text) => Loaded::Search {
            text: text.clone(),
            results: src.search(text, SEARCH_LIMIT)?,
        },
        // A page of a shuffle: those items, wherever the listing starts.
        Load::Grid(q) if !q.ids.is_empty() => Loaded::Grid {
            page: src.items(&CatalogQuery {
                start: 0,
                limit: q.ids.len(),
                ..q.clone()
            })?,
            append: q.start > 0,
            order: None,
            by_id: true,
        },
        // Server shuffles differ page to page: its order is asked for once.
        Load::Grid(q) if q.sort == CatalogSort::Random => {
            let order = src.shuffled(q)?;
            let first: Vec<String> = order.iter().take(q.limit).cloned().collect();
            let items = if first.is_empty() {
                Vec::new()
            } else {
                src.items(&CatalogQuery {
                    start: 0,
                    limit: first.len(),
                    ids: first,
                    ..q.clone()
                })?
                .items
            };
            Loaded::Grid {
                page: CatalogPage {
                    total: order.len(),
                    items,
                },
                append: false,
                order: Some(order),
                by_id: true,
            }
        }
        Load::Grid(q) => Loaded::Grid {
            page: src.items(q)?,
            append: q.start > 0,
            order: None,
            by_id: false,
        },
        Load::Facets(q) => Loaded::Facets(src.facets(q).map_err(|e| super::errors::source(&e))),
        Load::Series { id, season } => {
            let series = Box::new(src.item(id)?);
            let seasons = src.seasons(id)?;
            let next_up = src
                .next_up(Some(id), 1)
                .ok()
                .and_then(|n| n.into_iter().next())
                .map(Box::new);
            let season = season
                .clone()
                .or_else(|| next_up.as_ref().and_then(|e| e.season_id.clone()))
                .or_else(|| seasons.first().map(|s| s.id.clone()));
            let episodes = src.episodes(id, season.as_deref())?;
            Loaded::Series {
                series,
                seasons,
                season,
                episodes,
                next_up,
            }
        }
        Load::Episodes { series, season } => Loaded::Episodes(src.episodes(series, Some(season))?),
        Load::Item { id } => Loaded::Item(Box::new(src.item(id)?)),
    })
}

impl CatalogView {
    /// The server's front page, loading under `seq`.
    pub fn new(seq: u64) -> CatalogView {
        CatalogView {
            pages: vec![Page {
                seq,
                request_seq: None,
                loading: true,
                error: None,
                body: Body::Home {
                    home: None,
                    search: String::new(),
                    typed: None,
                },
            }],
        }
    }

    /// Starts a request without changing the page's identity. Facet loads
    /// remain independent, so changing a filter cannot strand the drawer.
    pub fn begin_request(&mut self, seq: u64, request_seq: u64, load: &Load) -> Option<u64> {
        let page = self.pages.iter_mut().find(|page| page.seq == seq)?;
        if matches!(load, Load::Facets(_)) {
            if let Body::Grid(grid) = &mut page.body {
                grid.facets = Facets::Asked;
            }
            return Some(page.seq);
        }
        page.request_seq = Some(request_seq);
        page.loading = true;
        Some(request_seq)
    }

    /// Gives an answer to its page, discarding superseded data loads.
    pub fn loaded(&mut self, seq: u64, result: Result<Loaded, String>) {
        let facets = matches!(&result, Ok(Loaded::Facets(_)));
        let Some(p) = self.pages.iter_mut().find(|p| {
            if facets {
                p.seq == seq
            } else {
                p.request_seq.unwrap_or(p.seq) == seq
            }
        }) else {
            return;
        };
        // The drawer's choices come beside the page's own answers.
        if !facets {
            p.loading = false;
        }
        let loaded = match result {
            Ok(l) => l,
            Err(e) => {
                p.error = Some(e);
                return;
            }
        };
        if !facets {
            p.error = None;
        }
        match (&mut p.body, loaded) {
            (Body::Home { home, .. }, Loaded::Home(h)) => *home = Some(h),
            (Body::Search { asked, results, .. }, Loaded::Search { text, results: r }) => {
                *asked = text;
                *results = r;
            }
            (
                Body::Grid(g),
                Loaded::Grid {
                    page,
                    append,
                    order,
                    by_id,
                },
            ) => {
                if append {
                    g.items.extend(page.items);
                } else {
                    g.items = page.items;
                }
                if order.is_some() {
                    g.order = order;
                } else if !by_id && !append {
                    g.order = None;
                }
                g.total = match &g.order {
                    Some(o) => o.len(),
                    None => page.total.max(g.items.len()),
                };
            }
            (Body::Grid(g), Loaded::Facets(f)) => {
                g.facets = match f {
                    Ok(f) => Facets::Got(f),
                    Err(e) => Facets::Failed(e),
                }
            }
            (
                Body::Series {
                    series,
                    seasons,
                    season,
                    episodes,
                    next_up,
                },
                Loaded::Series {
                    series: s,
                    seasons: ss,
                    season: chosen,
                    episodes: es,
                    next_up: n,
                },
            ) => {
                *series = s;
                *seasons = ss;
                *season = chosen;
                *episodes = es;
                *next_up = n;
            }
            (Body::Series { episodes, .. }, Loaded::Episodes(es)) => *episodes = es,
            (Body::Item { item }, Loaded::Item(i)) => *item = i,
            _ => {}
        }
    }

    /// What brings the page shown up to date (after watching something).
    /// Opens a page, loading until answer `seq` arrives.
    fn open(&mut self, seq: u64, body: Body) {
        self.pages.push(Page {
            seq,
            request_seq: None,
            loading: true,
            error: None,
            body,
        });
    }

    pub fn reload(&mut self) -> Option<(u64, Load)> {
        let p = self.pages.last_mut()?;
        p.loading = true;
        let load = match &p.body {
            Body::Home { .. } => Load::Home,
            Body::Search { asked, .. } => Load::Search(asked.clone()),
            Body::Grid(g) => Load::Grid(match &g.order {
                // The same shuffle, brought up to date.
                Some(o) => CatalogQuery {
                    start: 0,
                    ids: o.iter().take(g.items.len().max(PAGE)).cloned().collect(),
                    ..g.query.clone()
                },
                None => CatalogQuery {
                    start: 0,
                    limit: g.items.len().max(PAGE),
                    ..g.query.clone()
                },
            }),
            Body::Series { series, season, .. } => Load::Series {
                id: series.id.clone(),
                season: season.clone(),
            },
            Body::Item { item } => Load::Item {
                id: item.id.clone(),
            },
        };
        Some((p.seq, load))
    }

    /// Whether the page shown has its Filters drawer open.
    pub fn filters_open(&self) -> bool {
        matches!(self.pages.last().map(|p| &p.body), Some(Body::Grid(g)) if g.filters_open)
    }

    /// Opens or closes the Filters drawer of the page shown, a grid.
    pub fn show_filters(&mut self, open: bool) {
        if let Some(Body::Grid(g)) = self.pages.last_mut().map(|p| &mut p.body) {
            g.filters_open = open;
        }
    }

    /// Shows a mark at once wherever the item appears; the server is told
    /// separately.
    fn mark(&mut self, id: &str, mark: Mark, on: bool) {
        let set = |i: &mut CatalogItem| {
            if i.id != id {
                return;
            }
            match mark {
                Mark::Favorite => i.favorite = on,
                Mark::Played => {
                    i.played = on;
                    i.position = 0.0;
                }
            }
        };
        for p in &mut self.pages {
            match &mut p.body {
                Body::Home { home: Some(h), .. } => {
                    for i in h
                        .resume
                        .iter_mut()
                        .chain(h.next_up.iter_mut())
                        .chain(h.latest.iter_mut().flat_map(|(_, l)| l.iter_mut()))
                    {
                        set(i);
                    }
                }
                Body::Search { results, .. } => results
                    .iter_mut()
                    .flat_map(|(_, items)| items.iter_mut())
                    .for_each(set),
                Body::Grid(g) => g.items.iter_mut().for_each(set),
                Body::Series {
                    series,
                    episodes,
                    next_up,
                    ..
                } => {
                    set(series);
                    episodes.iter_mut().for_each(set);
                    if let Some(n) = next_up {
                        set(n);
                    }
                }
                Body::Item { item } => set(item),
                _ => {}
            }
        }
    }
}

/// What the page asked for while it was drawn.
enum Go {
    /// Into a library, collection, folder, series or season.
    Open(CatalogItem),
    /// Item `.1` of a list, the rest queued; `.2` from the start.
    Play(Vec<CatalogItem>, usize, bool),
    /// Play next (`true`) or Add to queue.
    Queue(CatalogItem, bool),
    AddToPlaylist(CatalogItem),
    /// Everything on the server with this tag.
    Tag(String),
    Details(CatalogItem),
    /// Load again what the page shows (`Load` with the page's number).
    Reload(Load),
    /// The Filters drawer's choices, beside what the page shows.
    Facets(CatalogQuery),
    Mark(String, Mark, bool),
    /// Play the item at this location from this version (`None`: the
    /// tallest that fits).
    Version(String, Option<String>),
    Search(String),
}

/// A server's page; true when Back was clicked.
pub fn page(
    ui: &mut egui::Ui,
    v: &mut View,
    source: &str,
    name: &str,
    j: &mut CatalogView,
) -> bool {
    let caps = v
        .services
        .opener
        .source(source)
        .and_then(|src| src.as_catalog().map(Catalog::capabilities))
        .unwrap_or_default();
    let subtitle = trail(&j.pages);
    let Some(p) = j.pages.last_mut() else {
        return true;
    };
    let mut refresh = false;
    let back = page::header(ui, name, Some(&subtitle), |ui| {
        refresh = widgets::icon_button(ui, icons::ARROWS_CLOCKWISE, 44.0, false)
            .tip(tr!("feed-refresh"))
            .clicked();
        if let Body::Home { search, typed, .. } | Body::Search { search, typed, .. } = &mut p.body
            && page::search_field(ui, search, tr!("jf-search-server"), 220.0)
        {
            *typed = Some(Instant::now());
        }
    });
    if refresh {
        if let Some((seq, load)) = j.reload() {
            v.actions.push(Action::Catalog {
                source: source.to_string(),
                seq,
                load,
            });
        }
        return back;
    }
    let Some(p) = j.pages.last_mut() else {
        return true;
    };
    page::status(ui, p.loading && !has_content(&p.body), p.error.as_deref());
    let loading = p.loading;
    let top = match &mut p.body {
        Body::Grid(g) => std::mem::take(&mut g.to_top),
        _ => false,
    };
    let mut go: Option<Go> = None;
    page::scroll_from(ui, top, |ui| {
        go = match &mut p.body {
            Body::Home {
                home,
                search,
                typed,
            } => {
                if let Some(t) = *typed
                    && t.elapsed() >= SEARCH_DELAY
                {
                    *typed = None;
                    (!search.trim().is_empty()).then(|| Go::Search(std::mem::take(search)))
                } else {
                    if typed.is_some() {
                        ui.ctx().request_repaint_after(SEARCH_DELAY);
                    }
                    home.as_ref().and_then(|h| home_rows(ui, v, h))
                }
            }
            Body::Search {
                search,
                typed,
                asked,
                results,
            } => search_results(ui, v, search, typed, asked, results, loading),
            Body::Grid(g) => grid(ui, v, g, loading, caps),
            Body::Series {
                series,
                seasons,
                season,
                episodes,
                next_up,
            } => series_page(
                ui,
                v,
                series,
                seasons,
                season,
                episodes,
                next_up.as_deref(),
                caps,
            ),
            Body::Item { item } => details(ui, v, item, caps),
        };
    });
    if let Some(go) = go {
        follow(v, source, j, go);
    }
    back
}

/// Carries out what the page asked for.
fn follow(v: &mut View, source: &str, j: &mut CatalogView, go: Go) {
    let ask = |v: &mut View, seq: u64, load: Load| {
        v.actions.push(Action::Catalog {
            source: source.to_string(),
            seq,
            load,
        });
    };
    match go {
        Go::Reload(load) => {
            if let Some(p) = j.pages.last_mut() {
                p.loading = true;
                ask(v, p.seq, load);
            }
        }
        Go::Facets(query) => {
            if let Some(p) = j.pages.last() {
                ask(v, p.seq, Load::Facets(query));
            }
        }
        Go::Play(list, i, from_start) => {
            let reqs: Vec<OpenRequest> = list
                .iter()
                .enumerate()
                .map(|(k, item)| request(source, item, from_start && k == i))
                .collect();
            v.actions.push(Action::OpenList(reqs, i));
        }
        Go::Queue(item, next) => v.actions.push(Action::Queue {
            req: request(source, &item, false),
            next,
        }),
        Go::AddToPlaylist(item) => {
            v.state.picking = Some(super::Picking {
                reqs: vec![request(source, &item, false)],
                title: item.name.clone(),
            });
        }
        Go::Mark(id, mark, on) => {
            j.mark(&id, mark, on);
            v.actions.push(Action::CatalogMark {
                source: source.to_string(),
                id,
                mark,
                on,
            });
        }
        Go::Version(location, version) => v.actions.push(Action::CatalogVersion {
            source: source.to_string(),
            location,
            version,
        }),
        Go::Details(item) => {
            let seq = v.state.next_seq();
            ask(
                v,
                seq,
                Load::Item {
                    id: item.id.clone(),
                },
            );
            j.open(
                seq,
                Body::Item {
                    item: Box::new(item),
                },
            );
        }
        Go::Search(text) => {
            let seq = v.state.next_seq();
            let text = text.trim().to_string();
            ask(v, seq, Load::Search(text.clone()));
            j.open(
                seq,
                Body::Search {
                    search: text.clone(),
                    typed: None,
                    asked: text,
                    results: Vec::new(),
                },
            );
        }
        Go::Open(item) => {
            let seq = v.state.next_seq();
            let (load, body) = page_for(item);
            ask(v, seq, load);
            j.open(seq, body);
        }
        Go::Tag(tag) => {
            let seq = v.state.next_seq();
            let query = CatalogQuery {
                limit: PAGE,
                ..CatalogQuery::tagged(&tag)
            };
            let title = tr!("jf-tagged", tag = tag.as_str());
            let body = Body::Grid(Box::new(Grid::new(title, query.clone())));
            ask(v, seq, Load::Grid(query));
            j.open(seq, body);
        }
    }
}

/// What opening a library, collection, folder, series or season loads,
/// and the page that shows it.
fn page_for(item: CatalogItem) -> (Load, Body) {
    match item.kind {
        CatalogKind::Series | CatalogKind::Season => {
            let (id, season) = if item.kind == CatalogKind::Season {
                (
                    item.series_id.clone().unwrap_or_default(),
                    Some(item.id.clone()),
                )
            } else {
                (item.id.clone(), None)
            };
            let load = Load::Series {
                id: id.clone(),
                season: season.clone(),
            };
            let series = CatalogItem {
                id,
                name: item.series_name.clone().unwrap_or(item.name.clone()),
                kind: CatalogKind::Series,
                ..item
            };
            let body = Body::Series {
                series: Box::new(series),
                seasons: Vec::new(),
                season,
                episodes: Vec::new(),
                next_up: None,
            };
            (load, body)
        }
        _ => {
            let query = CatalogQuery {
                limit: PAGE,
                ..CatalogQuery::children(&item)
            };
            let body = Body::Grid(Box::new(Grid::new(item.name.clone(), query.clone())));
            (Load::Grid(query), body)
        }
    }
}

/// Where the page shown is on the server, as the header's second line: the
/// pages opened to reach it ("Movies › Blade Runner", the last three), or
/// what the server is on its front page.
fn trail(pages: &[Page]) -> String {
    let parts: Vec<String> = pages
        .iter()
        .filter_map(|p| match &p.body {
            Body::Home { .. } => None,
            Body::Search { asked, .. } => Some(tr!("jf-trail-search", text = asked.as_str())),
            Body::Grid(g) => Some(g.title.clone()),
            Body::Series { series, .. } => Some(series.name.clone()),
            Body::Item { item } => Some(item.name.clone()),
        })
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    parts[parts.len().saturating_sub(3)..].join(&format!("  {}  ", icons::CARET_RIGHT))
}

/// The page shows something already (a reload keeps it on screen).
fn has_content(body: &Body) -> bool {
    match body {
        Body::Home { home, .. } => home.is_some(),
        Body::Search { results, .. } => !results.is_empty(),
        Body::Grid(g) => !g.items.is_empty(),
        Body::Series { episodes, .. } => !episodes.is_empty(),
        Body::Item { .. } => true,
    }
}

/// Opens `item`, starting from the server's resume point.
fn request(source: &str, item: &CatalogItem, from_start: bool) -> OpenRequest {
    OpenRequest {
        location: item.location.clone(),
        source_id: Some(source.to_string()),
        entry: Some(item.to_entry()),
        title: None,
        start_at: if from_start {
            Some(0.0)
        } else {
            (item.position > 1.0).then_some(item.position)
        },
        ..Default::default()
    }
}

/// "1982 · 1:57:00", "S1:E3 · Westworld", "4 seasons".
fn card_facts(item: &CatalogItem) -> String {
    let mut f = Vec::new();
    match item.kind {
        CatalogKind::Episode => {
            if let Some(e) = item.episode_label() {
                f.push(e);
            }
            if let Some(s) = &item.series_name {
                f.push(s.clone());
            }
        }
        CatalogKind::Series => {
            if let Some(y) = item.year {
                f.push(y.to_string());
            }
            match item.unplayed {
                Some(n) if n > 0 => f.push(tr!("jf-unwatched", count = n)),
                _ => {}
            }
        }
        CatalogKind::BoxSet | CatalogKind::Folder | CatalogKind::Library => {
            if let Some(n) = item.child_count {
                f.push(tr!("count-items", count = (n as usize)));
            }
        }
        _ => {
            if let Some(y) = item.year {
                f.push(y.to_string());
            }
        }
    }
    if item.kind.playable()
        && let Some(d) = item.runtime
    {
        f.push(fmt_time(d));
    }
    f.join("  ·  ")
}

/// A VR format worth a badge (flat films get none).
fn vr_badge(item: &CatalogItem) -> Option<String> {
    format_badge(item.format)
}

/// `format` when it is VR or 3D.
fn format_badge(format: Option<fp_core::format::VideoFormat>) -> Option<String> {
    let f = format?;
    let flat = f.projection == fp_core::format::Projection::Flat
        && f.stereo == fp_core::format::StereoLayout::Mono;
    (!flat).then(|| widgets::format_short(&f))
}

/// An item's card, a poster or 16:9 art.
fn item_card(
    ui: &mut egui::Ui,
    v: &mut View,
    item: &CatalogItem,
    portrait: bool,
) -> page::CardResponse {
    let thumb = if portrait {
        item.poster.clone()
    } else {
        item.landscape.clone().or_else(|| item.poster.clone())
    };
    let facts = card_facts(item);
    let badge = vr_badge(item);
    let placeholder = match item.kind {
        CatalogKind::Series | CatalogKind::Season => icons::MONITOR_PLAY,
        CatalogKind::Library | CatalogKind::Folder | CatalogKind::BoxSet => icons::FOLDERS,
        CatalogKind::Person => icons::USER,
        _ => icons::FILM_STRIP,
    };
    page::card(
        ui,
        v.thumbs,
        &Card {
            size: if portrait {
                page::PORTRAIT
            } else {
                page::LANDSCAPE
            },
            thumb: thumb.as_deref(),
            placeholder,
            title: &item.name,
            facts: &facts,
            badge: badge.as_deref(),
            progress: item.progress(),
            watched: item.played,
            favorite: item.favorite,
            more: item.kind.playable(),
            play: item.kind.playable(),
            ..Card::default()
        },
    )
}

/// What clicking a card in `list` does.
fn card_go(c: page::CardResponse, list: &[CatalogItem], i: usize) -> Option<Go> {
    let item = &list[i];
    if c.more {
        Some(Go::Details(item.clone()))
    } else if c.open && item.kind.playable() {
        Some(Go::Play(list.to_vec(), i, false))
    } else if c.open {
        Some(Go::Open(item.clone()))
    } else {
        None
    }
}

/// A titled row of cards.
fn item_row(
    ui: &mut egui::Ui,
    v: &mut View,
    title: &str,
    items: &[CatalogItem],
    landscape: bool,
) -> Option<Go> {
    let mut go = None;
    page::carousel(ui, title, None, items.len(), |ui, i| {
        let portrait = !landscape && items[i].kind.portrait();
        let c = item_card(ui, v, &items[i], portrait);
        go = go.take().or_else(|| card_go(c, items, i));
    });
    go
}

fn home_rows(ui: &mut egui::Ui, v: &mut View, h: &CatalogHome) -> Option<Go> {
    ui.add_space(6.0);
    let mut go = item_row(ui, v, tr!("jf-my-media"), &h.libraries, true);
    go = go.or(item_row(ui, v, tr!("jf-continue"), &h.resume, true));
    go = go.or(item_row(ui, v, tr!("jf-next-up"), &h.next_up, true));
    for (library, items) in &h.latest {
        let title = tr!("jf-latest-in", library = library.name.as_str());
        go = go.or(item_row(ui, v, &title, items, false));
    }
    if h.libraries.is_empty() {
        page::empty_state(
            ui,
            icons::MONITOR_PLAY,
            tr!("jf-no-libraries"),
            tr!("jf-no-libraries-desc"),
            |_| {},
        );
    }
    go
}

/// Search results, a row per kind. Typing in the header's search field
/// searches again on this page.
fn search_results(
    ui: &mut egui::Ui,
    v: &mut View,
    search: &str,
    typed: &mut Option<Instant>,
    asked: &str,
    results: &[(&'static str, Vec<CatalogItem>)],
    loading: bool,
) -> Option<Go> {
    if let Some(t) = *typed {
        if t.elapsed() >= SEARCH_DELAY {
            *typed = None;
            let text = search.trim();
            if !text.is_empty() && text != asked {
                return Some(Go::Reload(Load::Search(text.to_string())));
            }
        } else {
            ui.ctx().request_repaint_after(SEARCH_DELAY);
        }
    }
    ui.add_space(6.0);
    let mut go = None;
    for (section, items) in results {
        go = go.or(item_row(ui, v, section, items, false));
    }
    if results.is_empty() && !loading {
        page::empty_state(
            ui,
            icons::MAGNIFYING_GLASS,
            tr!("jf-nothing-found"),
            &tr!("jf-nothing-found-desc", text = asked),
            |_| {},
        );
    }
    go
}

/// A searchable, sortable, filterable listing, loading more as its end
/// shows.
fn grid(
    ui: &mut egui::Ui,
    v: &mut View,
    g: &mut Grid,
    loading: bool,
    caps: CatalogCapabilities,
) -> Option<Go> {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if page::search_field(ui, &mut g.query.search, tr!("jf-search"), 220.0) {
            g.typed = Some(Instant::now());
        }
        if page::filters_button(ui, g.on().len()).clicked() {
            g.filters_open = !g.filters_open;
        }
        ui.add_space(10.0);
        for (f, label) in [
            (CatalogFilter::All, tr!("filter-all")),
            (CatalogFilter::Unplayed, tr!("filter-unwatched")),
            (CatalogFilter::Resumable, tr!("filter-in-progress")),
            (CatalogFilter::Favorites, tr!("filter-favorites")),
        ] {
            if f == CatalogFilter::Favorites && !caps.favorites {
                continue;
            }
            if widgets::chip(ui, label, g.query.filter == f).clicked() && g.query.filter != f {
                g.query.filter = f;
                changed = true;
            }
        }
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if page::sort_bar(
            ui,
            &[
                (CatalogSort::Name, tr!("sort-title")),
                (CatalogSort::Added, tr!("sort-date-added")),
                (CatalogSort::Released, tr!("sort-release-date")),
                (CatalogSort::Rating, tr!("filter-rating")),
                (CatalogSort::Runtime, tr!("filter-length")),
                (CatalogSort::LastWatched, tr!("sort-last-watched")),
                (CatalogSort::Random, tr!("sort-shuffle")),
            ],
            &mut g.query.sort,
        ) {
            g.query.descending = g.query.sort.default_descending();
            changed = true;
        }
        changed |= if g.query.sort == CatalogSort::Random {
            page::shuffle_again(ui)
        } else {
            page::direction(
                ui,
                &mut g.query.descending,
                [tr!("sort-largest-first"), tr!("sort-smallest-first")],
            )
        };
        page::count_label(ui, &tr!("count-items", count = g.total));
    });
    match page::active_filters(ui, &g.labels()) {
        Some(page::FilterEdit::Remove(i)) => {
            g.take_off(i);
            changed = true;
        }
        Some(page::FilterEdit::Clear) => {
            g.clear_filters();
            changed = true;
        }
        None => {}
    }
    ui.add_space(10.0);
    if let Some(t) = g.typed {
        if t.elapsed() >= SEARCH_DELAY {
            g.typed = None;
            changed = true;
        } else {
            ui.ctx().request_repaint_after(SEARCH_DELAY);
        }
    }
    let mut go = None;
    if g.filters_open {
        if let Some(query) = g.unasked_facets() {
            go = Some(Go::Facets(query));
        }
        let mut open = true;
        let count = tr!("count-items", count = g.total);
        let clear = page::filter_drawer(ui, &mut open, &count, |ui| changed |= drawer(ui, g, caps));
        g.filters_open = open;
        if clear {
            g.clear_filters();
            changed = true;
        }
    }
    if changed {
        return Some(Go::Reload(Load::Grid(g.restart())));
    }
    if g.items.is_empty() {
        return go;
    }
    let items = &g.items;
    let portrait = items.iter().filter(|i| i.kind.portrait()).count() * 2 >= items.len();
    let width = if portrait {
        page::PORTRAIT.x
    } else {
        page::LANDSCAPE.x
    };
    page::grid(ui, items.len(), width, |ui, i| {
        let c = item_card(ui, v, &items[i], portrait);
        go = go.take().or_else(|| card_go(c, items, i));
    });
    if items.len() < g.total && page::load_more(ui, loading) {
        go = Some(Go::Reload(Load::Grid(g.next_page())));
    }
    go
}

/// The Filters drawer's sections, those the library has choices for; true
/// when a filter changed.
fn drawer(ui: &mut egui::Ui, g: &mut Grid, caps: CatalogCapabilities) -> bool {
    let facets = match std::mem::replace(&mut g.facets, Facets::Asked) {
        Facets::Got(f) => f,
        other => {
            match &other {
                Facets::Failed(e) => {
                    ui.label(
                        RichText::new(format!("{}  {e}", icons::WARNING_CIRCLE))
                            .color(theme::ERROR),
                    );
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(tr!("jf-asking")).color(theme::TEXT_2));
                    });
                }
            }
            g.facets = other;
            return false;
        }
    };
    let mut changed = false;
    let toggle = |list: &mut Vec<String>, name: &str| match list.iter().position(|x| x == name) {
        Some(i) => {
            list.remove(i);
        }
        None => list.push(name.to_string()),
    };
    let q = &mut g.query;
    let lists = &mut g.lists;
    let mut list = |ui: &mut egui::Ui,
                    title: &'static str,
                    hint: &str,
                    names: &[&str],
                    chosen: &mut Vec<String>| {
        if names.is_empty() {
            return false;
        }
        let choices: Vec<(&str, Option<usize>)> = names.iter().map(|n| (*n, None)).collect();
        let state = lists.entry(title).or_default();
        let on = |n: &str| chosen.iter().any(|c| c == n);
        let picked = page::filter_list(ui, title, hint, &choices, on, state);
        if let Some(name) = picked {
            toggle(chosen, name);
        }
        picked.is_some()
    };
    let genres: Vec<&str> = facets.genres.iter().map(String::as_str).collect();
    changed |= list(
        ui,
        tr!("filter-genres"),
        tr!("filter-find-genre"),
        &genres,
        &mut q.genres,
    );
    let decades = decades(&facets.years);
    if !decades.is_empty() {
        let on = decades_on(&q.years);
        page::filter_section(ui, tr!("filter-released"), |ui| {
            for d in decades {
                if page::filter_chip(ui, &tr!("filter-decade", decade = d), None, on.contains(&d)) {
                    if on.contains(&d) {
                        q.years.retain(|y| !(d..d + 10).contains(y));
                    } else {
                        q.years.extend(d..d + 10);
                    }
                    changed = true;
                }
            }
        });
    }
    // Series and folders have no width of their own.
    let videos = caps.width_filter && (q.types.is_empty() || q.types.iter().any(|t| t.playable()));
    if videos {
        page::filter_section(ui, tr!("adjust-resolution"), |ui| {
            for (w, label) in RESOLUTIONS {
                let on = q.min_width == Some(w);
                if page::filter_chip(ui, label, None, on) {
                    q.min_width = (!on).then_some(w);
                    changed = true;
                }
            }
        });
    }
    let studios: Vec<&str> = facets.studios.iter().map(String::as_str).collect();
    changed |= list(
        ui,
        tr!("filter-studios"),
        tr!("filter-find-studio"),
        &studios,
        &mut q.studios,
    );
    // The page's own tag is no choice.
    let tags: Vec<&str> = facets
        .tags
        .iter()
        .filter(|t| caps.tags && !g.base_tags.contains(t))
        .map(String::as_str)
        .collect();
    changed |= list(
        ui,
        tr!("filter-tags"),
        tr!("filter-find-tag"),
        &tags,
        &mut q.tags,
    );
    if facets.genres.is_empty() && facets.studios.is_empty() && tags.is_empty() && !videos {
        ui.label(RichText::new(tr!("jf-no-filters")).color(theme::TEXT_2));
    }
    g.facets = Facets::Got(facets);
    changed
}

/// Year, rating and other facts about an item.
fn item_facts(item: &CatalogItem) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(y) = item.year {
        f.push(y.to_string());
    }
    if item.kind.playable()
        && let Some(d) = item.runtime
    {
        f.push(fmt_time(d));
    }
    if let Some(r) = &item.official_rating {
        f.push(r.clone());
    }
    if let Some(r) = item.community_rating {
        f.push(page::rating(r));
    }
    if let Some(b) = format_badge(item.shown_format()) {
        f.push(b);
    }
    if item.played {
        f.push(tr!("jf-watched").into());
    }
    f
}

/// The favourite and watched toggles.
fn mark_chips(ui: &mut egui::Ui, item: &CatalogItem, caps: CatalogCapabilities) -> Option<Go> {
    let mut go = None;
    let heart = if item.favorite {
        tr!("feed-favorite")
    } else {
        tr!("feed-add-favorite")
    };
    if caps.favorites && widgets::chip_icon(ui, Some(icons::HEART), heart, item.favorite).clicked()
    {
        go = Some(Go::Mark(item.id.clone(), Mark::Favorite, !item.favorite));
    }
    if widgets::chip_icon(ui, Some(icons::CHECK), tr!("jf-watched"), item.played).clicked() {
        go = Some(Go::Mark(item.id.clone(), Mark::Played, !item.played));
    }
    go
}

/// The start of a long text, cut at a word.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let cut = cut.rsplit_once(' ').map(|(a, _)| a).unwrap_or(&cut);
    format!("{}…", cut.trim_end_matches([',', '.', ';']))
}

#[allow(clippy::too_many_arguments)]
fn series_page(
    ui: &mut egui::Ui,
    v: &mut View,
    series: &CatalogItem,
    seasons: &[CatalogItem],
    season: &mut Option<String>,
    episodes: &[CatalogItem],
    next_up: Option<&CatalogItem>,
    caps: CatalogCapabilities,
) -> Option<Go> {
    let mut go = None;
    ui.add_space(4.0);
    let key = series
        .backdrop
        .clone()
        .or_else(|| series.landscape.clone())
        .or_else(|| series.poster.clone());
    page::banner(ui, v.thumbs, key.as_deref(), BANNER, |inner| {
        inner.add_space(14.0);
        inner.add(
            egui::Label::new(
                RichText::new(&series.name)
                    .font(theme::font(Weight::Bold, 40.0))
                    .color(Color32::WHITE),
            )
            .truncate(),
        );
        let mut f = item_facts(series);
        if !seasons.is_empty() {
            f.push(tr!("count-seasons", count = seasons.len()));
        }
        page::facts(inner, &f);
        if let Some(o) = &series.overview {
            inner.add_space(4.0);
            page::prose(inner, &clip(o, 220));
        }
        inner.add_space(14.0);
        inner.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            if let Some(n) = next_up {
                let ep = n.episode_label().unwrap_or_default();
                let label = if n.position > 1.0 {
                    tr!("jf-resume-episode", episode = ep.as_str())
                } else {
                    tr!("jf-play-episode", episode = ep.as_str())
                };
                if widgets::button(ui, Some(icons::PLAY), &label, Kind::Primary).clicked() {
                    // The next episode, with the rest of its season after it
                    // when it is the season shown.
                    go = Some(match episodes.iter().position(|e| e.id == n.id) {
                        Some(i) => Go::Play(episodes.to_vec(), i, false),
                        None => Go::Play(vec![n.clone()], 0, false),
                    });
                }
            }
            go = go.take().or(mark_chips(ui, series, caps));
        });
    });
    ui.add_space(18.0);
    if seasons.len() > 1 {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            for s in seasons {
                let selected = season.as_deref() == Some(s.id.as_str());
                if widgets::chip(ui, &s.name, selected).clicked() && !selected {
                    *season = Some(s.id.clone());
                    go = Some(Go::Reload(Load::Episodes {
                        series: series.id.clone(),
                        season: s.id.clone(),
                    }));
                }
            }
        });
        ui.add_space(12.0);
    }
    for (i, e) in episodes.iter().enumerate() {
        let (play, more) = episode_row(ui, v, e);
        if play {
            go = Some(Go::Play(episodes.to_vec(), i, false));
        }
        if more {
            go = Some(Go::Details(e.clone()));
        }
    }
    ui.add_space(20.0);
    go
}

/// One episode: still, number and title, facts, the start of its story;
/// returns (play clicked, details clicked).
fn episode_row(ui: &mut egui::Ui, v: &mut View, e: &CatalogItem) -> (bool, bool) {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 132.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return (false, false);
    }
    let t = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(14),
        theme::SURFACE.gamma_multiply(t),
    );
    let thumb =
        egui::Rect::from_min_size(rect.min + Vec2::new(10.0, 12.0), Vec2::new(192.0, 108.0));
    page::picture(
        ui,
        v.thumbs,
        e.landscape.as_deref().or(e.poster.as_deref()),
        thumb,
        10,
        icons::FILM_STRIP,
    );
    let painter = ui.painter();
    if let Some(p) = e.progress().filter(|p| *p > 0.005) {
        let bar = egui::Rect::from_min_max(
            egui::pos2(thumb.left() + 8.0, thumb.bottom() - 8.0),
            egui::pos2(thumb.right() - 8.0, thumb.bottom() - 4.0),
        );
        painter.rect_filled(
            bar,
            egui::CornerRadius::same(2),
            Color32::from_black_alpha(140),
        );
        let mut done = bar;
        done.set_width(bar.width() * p);
        painter.rect_filled(done, egui::CornerRadius::same(2), theme::ACCENT);
    }
    if t > 0.01 {
        painter.circle_filled(
            thumb.center(),
            24.0,
            Color32::from_black_alpha((t * 150.0) as u8),
        );
        widgets::play_mark(
            painter,
            thumb.center(),
            20.0,
            Color32::from_white_alpha((t * 255.0) as u8),
        );
    }
    let left = thumb.right() + 18.0;
    let width = rect.right() - left - 70.0;
    let text = |s: String, font: egui::FontId, color: Color32, rows: usize| {
        let mut job =
            egui::text::LayoutJob::single_section(s, egui::TextFormat::simple(font, color));
        job.wrap = egui::text::TextWrapping {
            max_width: width,
            max_rows: rows,
            break_anywhere: false,
            overflow_character: Some('…'),
        };
        ui.fonts(|f| f.layout_job(job))
    };
    let title = match e.index {
        Some(n) => format!("{n}. {}", e.name),
        None => e.name.clone(),
    };
    let mut facts = Vec::new();
    if let Some(d) = e.runtime {
        facts.push(fmt_time(d));
    }
    if e.position > 1.0
        && let Some(d) = e.runtime
    {
        let left = fmt_time(d - e.position);
        facts.push(tr!("jf-time-left", time = left));
    }
    if let Some(b) = vr_badge(e) {
        facts.push(b);
    }
    let title = text(title, theme::font(Weight::SemiBold, 18.0), theme::TEXT, 1);
    let facts = text(
        facts.join("  ·  "),
        theme::font(Weight::Regular, 15.0),
        theme::TEXT_2,
        1,
    );
    let story = text(
        e.overview.clone().unwrap_or_default(),
        theme::font(Weight::Regular, 15.0),
        theme::TEXT_3,
        2,
    );
    let painter = ui.painter();
    painter.galley(egui::pos2(left, rect.top() + 14.0), title, theme::TEXT);
    painter.galley(egui::pos2(left, rect.top() + 40.0), facts, theme::TEXT_2);
    painter.galley(egui::pos2(left, rect.top() + 64.0), story, theme::TEXT_3);
    if e.played {
        painter.text(
            egui::pos2(rect.right() - 60.0, rect.center().y),
            Align2::CENTER_CENTER,
            icons::CHECK_CIRCLE,
            theme::icon_fill(26.0),
            theme::ACCENT_HOVER,
        );
    }
    let more = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 24.0, rect.center().y),
        Vec2::splat(36.0),
    );
    let more_resp = ui.interact(more, resp.id.with("more"), Sense::click());
    let m = ui
        .ctx()
        .animate_bool_with_time(more_resp.id, more_resp.hovered(), 0.12);
    let painter = ui.painter();
    painter.circle_filled(
        more.center(),
        18.0,
        Color32::from_white_alpha((m * 20.0) as u8),
    );
    painter.text(
        more.center(),
        Align2::CENTER_CENTER,
        icons::DOTS_THREE,
        theme::icon(24.0),
        theme::TEXT_2.lerp_to_gamma(Color32::WHITE, m.max(t * 0.6)),
    );
    let more_clicked = more_resp.tip(tr!("jf-details")).clicked();
    (resp.clicked() && !more_clicked, more_clicked)
}

/// A film's, episode's or video's page.
fn details(
    ui: &mut egui::Ui,
    v: &mut View,
    item: &mut CatalogItem,
    caps: CatalogCapabilities,
) -> Option<Go> {
    let mut go = None;
    ui.add_space(4.0);
    let key = item
        .backdrop
        .clone()
        .or_else(|| item.landscape.clone())
        .or_else(|| item.poster.clone());
    page::banner(ui, v.thumbs, key.as_deref(), BANNER, |inner| {
        inner.add_space(14.0);
        if let (Some(series), Some(ep)) = (&item.series_name, item.episode_label()) {
            widgets::section_label(inner, &format!("{series} · {ep}"));
        }
        inner.add(
            egui::Label::new(
                RichText::new(&item.name)
                    .font(theme::font(Weight::Bold, 40.0))
                    .color(Color32::WHITE),
            )
            .truncate(),
        );
        page::facts(inner, &item_facts(item));
        inner.add_space(16.0);
        inner.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            let resume = item.position > 1.0;
            let label = if resume {
                tr!("jf-resume-from", time = fmt_time(item.position))
            } else {
                tr!("player-play").to_string()
            };
            if widgets::button(ui, Some(icons::PLAY), &label, Kind::Primary).clicked() {
                go = Some(Go::Play(vec![item.clone()], 0, false));
            }
            if resume
                && widgets::button(
                    ui,
                    Some(icons::ARROW_COUNTER_CLOCKWISE),
                    tr!("jf-start-over"),
                    Kind::Secondary,
                )
                .clicked()
            {
                go = Some(Go::Play(vec![item.clone()], 0, true));
            }
        });
        if matches!(
            item.kind,
            CatalogKind::Movie
                | CatalogKind::Episode
                | CatalogKind::Video
                | CatalogKind::MusicVideo
        ) {
            inner.add_space(8.0);
            inner.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let button = |ui: &mut egui::Ui, icon, label| {
                    widgets::button(ui, Some(icon), label, Kind::Secondary).clicked()
                };
                if button(ui, icons::SKIP_FORWARD, tr!("jf-play-next")) {
                    go = Some(Go::Queue(item.clone(), true));
                }
                if button(ui, icons::QUEUE, tr!("jf-add-to-queue")) {
                    go = Some(Go::Queue(item.clone(), false));
                }
                if button(ui, icons::PLAYLIST, tr!("jf-add-to-playlist")) {
                    go = Some(Go::AddToPlaylist(item.clone()));
                }
            });
        }
        inner.add_space(8.0);
        inner.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            go = go.take().or(mark_chips(ui, item, caps));
            if item.kind == CatalogKind::Episode
                && let Some(id) = &item.series_id
                && widgets::chip_icon(ui, Some(icons::MONITOR_PLAY), tr!("jf-series"), false)
                    .clicked()
            {
                go = Some(Go::Open(CatalogItem {
                    id: id.clone(),
                    name: item.series_name.clone().unwrap_or_default(),
                    kind: CatalogKind::Series,
                    ..CatalogItem::default()
                }));
            }
        });
    });
    ui.add_space(20.0);
    ui.set_max_width(1100.0);
    if item.versions.len() > 1 {
        if let Some(version) = version_chips(ui, item) {
            item.version = version.clone();
            go = Some(Go::Version(item.location.clone(), version));
        }
        ui.add_space(14.0);
    }
    if let Some(o) = &item.overview {
        page::prose(ui, o);
        ui.add_space(14.0);
    }
    let line = |ui: &mut egui::Ui, label: &str, text: String| {
        if text.is_empty() {
            return;
        }
        widgets::section_label(ui, label);
        ui.label(RichText::new(text).color(theme::TEXT_2));
        ui.add_space(10.0);
    };
    line(ui, tr!("filter-genres"), item.genres.join("  ·  "));
    let (cast, crew): (Vec<_>, Vec<_>) = item
        .people
        .iter()
        .partition(|(_, role)| !matches!(role.as_str(), "Director" | "Writer" | "Producer"));
    line(
        ui,
        tr!("jf-cast"),
        cast.iter()
            .take(12)
            .map(|(n, r)| {
                if r.is_empty() || r == "Actor" {
                    n.clone()
                } else {
                    format!("{n} ({r})")
                }
            })
            .collect::<Vec<_>>()
            .join(",  "),
    );
    line(
        ui,
        tr!("jf-crew"),
        crew.iter()
            .map(|(n, r)| format!("{n} ({r})"))
            .collect::<Vec<_>>()
            .join(",  "),
    );
    line(ui, tr!("filter-studios"), item.studios.join("  ·  "));
    if caps.tags && !item.tags.is_empty() {
        widgets::section_label(ui, tr!("filter-tags"));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            for t in &item.tags {
                if widgets::chip(ui, t, false).clicked() {
                    go = Some(Go::Tag(t.clone()));
                }
            }
        });
    }
    ui.add_space(20.0);
    go
}

/// The versions the server keeps of a film or video (4K and 1080p, a
/// director's cut); the one clicked, to play it from wherever it is opened.
fn version_chips(ui: &mut egui::Ui, item: &CatalogItem) -> Option<Option<String>> {
    let mut chosen = None;
    widgets::section_label(ui, tr!("jf-version"));
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        if widgets::chip(ui, tr!("jf-version-automatic"), item.version.is_none())
            .tip(tr!("jf-version-automatic-tip"))
            .clicked()
        {
            chosen = Some(None);
        }
        for version in &item.versions {
            let on = item.version.as_deref() == Some(version.id.as_str());
            let label = match format_badge(version.format) {
                Some(b) => format!("{} · {b}", version.label()),
                None => version.label(),
            };
            if widgets::chip(ui, &label, on).clicked() {
                chosen = Some(Some(version.id.clone()));
            }
        }
    });
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: CatalogKind) -> CatalogItem {
        CatalogItem {
            id: id.into(),
            name: id.into(),
            kind,
            ..CatalogItem::default()
        }
    }

    #[test]
    fn answers_reach_the_page_that_asked() {
        let mut j = CatalogView::new(1);
        j.pages.push(Page {
            seq: 2,
            request_seq: None,
            loading: true,
            error: None,
            body: Body::Grid(Box::new(Grid::new(
                "Movies".into(),
                CatalogQuery {
                    parent: Some("lib".into()),
                    ..CatalogQuery::default()
                },
            ))),
        });
        let page = |n: usize| CatalogPage {
            items: (0..n)
                .map(|i| item(&i.to_string(), CatalogKind::Movie))
                .collect(),
            total: 100,
        };
        let grid = |page: CatalogPage, append: bool| Loaded::Grid {
            page,
            append,
            order: None,
            by_id: false,
        };
        j.loaded(2, Ok(grid(page(60), false)));
        j.loaded(2, Ok(grid(page(40), true)));
        // An answer for a page that is gone is dropped.
        j.loaded(9, Ok(grid(page(5), false)));
        let Body::Grid(g) = &j.pages[1].body else {
            panic!()
        };
        assert_eq!((g.items.len(), g.total), (100, 100));
        assert!(!j.pages[1].loading);
        // The home page is still waiting.
        assert!(j.pages[0].loading);
        j.loaded(1, Err("offline".into()));
        assert_eq!(j.pages[0].error.as_deref(), Some("offline"));

        // The header says where the page is.
        assert!(trail(&j.pages[..1]).is_empty());
        assert_eq!(trail(&j.pages), "Movies");

        // A reload covers everything shown.
        let (seq, load) = j.reload().unwrap();
        assert_eq!(seq, 2);
        let Load::Grid(q) = load else { panic!() };
        assert_eq!((q.start, q.limit), (0, 100));
    }

    #[test]
    fn late_unfiltered_answers_cannot_replace_a_studio_filtered_listing() {
        let mut view = CatalogView::new(1);
        let query = CatalogQuery::default();
        view.pages[0].body = Body::Grid(Box::new(Grid::new("Movies".into(), query.clone())));
        let old = view.begin_request(1, 2, &Load::Grid(query)).unwrap();
        let Body::Grid(grid) = &mut view.pages[0].body else {
            panic!()
        };
        grid.query.studios = vec!["Selected studio".into()];
        let query = grid.restart();
        let current = view
            .begin_request(1, 3, &Load::Grid(query.clone()))
            .unwrap();
        let facets = view.begin_request(1, 4, &Load::Facets(query)).unwrap();
        assert_eq!(facets, 1, "facets retain the page's independent identity");

        view.loaded(old, Err("an obsolete request failed".into()));
        assert!(view.pages[0].loading);
        assert!(view.pages[0].error.is_none());
        view.loaded(
            facets,
            Ok(Loaded::Facets(Ok(CatalogFacets {
                studios: vec!["Selected studio".into()],
                ..Default::default()
            }))),
        );
        assert!(view.pages[0].loading, "facets do not finish the listing");
        let reply = |id: &str| Loaded::Grid {
            page: CatalogPage {
                items: vec![item(id, CatalogKind::Movie)],
                total: 1,
            },
            append: false,
            order: None,
            by_id: false,
        };
        view.loaded(current, Ok(reply("filtered")));
        view.loaded(old, Ok(reply("unfiltered")));
        view.loaded(old, Err("another stale failure".into()));
        let page = &view.pages[0];
        assert_eq!(page.seq, 1);
        assert!(!page.loading);
        assert!(page.error.is_none());
        let Body::Grid(grid) = &page.body else {
            panic!()
        };
        assert_eq!(grid.items[0].id, "filtered");
        assert_eq!(grid.query.studios, ["Selected studio"]);
        assert!(matches!(&grid.facets, Facets::Got(f) if f.studios == ["Selected studio"]));

        view.loaded(current, Err("the current listing failed".into()));
        view.loaded(facets, Ok(Loaded::Facets(Ok(CatalogFacets::default()))));
        assert_eq!(
            view.pages[0].error.as_deref(),
            Some("the current listing failed")
        );
    }

    #[test]
    fn changing_filters_discards_old_shuffle_ids_and_in_flight_pages() {
        let mut view = CatalogView::new(1);
        let mut grid = Grid::new(
            "Movies".into(),
            CatalogQuery {
                sort: CatalogSort::Random,
                ..Default::default()
            },
        );
        grid.items = vec![item("old-first", CatalogKind::Movie)];
        grid.order = Some(vec!["old-first".into(), "old-next".into()]);
        grid.total = 2;
        let next = grid.next_page();
        assert_eq!(next.ids, ["old-next"]);
        view.pages[0].body = Body::Grid(Box::new(grid));
        let old_page = view.begin_request(1, 2, &Load::Grid(next)).unwrap();
        let Body::Grid(grid) = &mut view.pages[0].body else {
            panic!()
        };
        grid.query.studios = vec!["New studio".into()];
        let changed = grid.restart();
        assert!(changed.ids.is_empty());
        assert!(grid.order.is_none());
        let first_filter = view.begin_request(1, 3, &Load::Grid(changed)).unwrap();

        // Refreshing before the filtered response arrives must not reuse
        // IDs from the old shuffle, even though its cards are still visible.
        let (page, reload) = view.reload().unwrap();
        let Load::Grid(query) = &reload else { panic!() };
        assert_eq!(query.studios, ["New studio"]);
        assert!(query.ids.is_empty());
        let refreshed = view.begin_request(page, 4, &reload).unwrap();
        let reply = |id: &str, append: bool| Loaded::Grid {
            page: CatalogPage {
                items: vec![item(id, CatalogKind::Movie)],
                total: 1,
            },
            append,
            order: (!append).then(|| vec![id.into()]),
            by_id: true,
        };
        view.loaded(old_page, Ok(reply("old-next", true)));
        view.loaded(first_filter, Ok(reply("earlier-filter", false)));
        assert!(view.pages[0].loading);
        view.loaded(refreshed, Ok(reply("new-studio", false)));
        view.loaded(old_page, Ok(reply("old-next", true)));
        let Body::Grid(grid) = &view.pages[0].body else {
            panic!()
        };
        assert_eq!(grid.items.len(), 1);
        assert_eq!(grid.items[0].id, "new-studio");
        assert_eq!(grid.order.as_deref().unwrap(), ["new-studio"]);
    }

    #[test]
    fn a_reload_that_supersedes_a_facet_action_does_not_strand_the_drawer() {
        let mut view = CatalogView::new(1);
        let mut grid = Grid::new("Movies".into(), CatalogQuery::default());
        grid.filters_open = true;
        let first_facets = grid.unasked_facets().unwrap();

        // The drawer offers a facets action, but the search debounce
        // expires in the same frame and its reload takes precedence.
        grid.query.search = "film".into();
        let query = grid.restart();
        view.pages[0].body = Body::Grid(Box::new(grid));
        view.begin_request(1, 2, &Load::Grid(query)).unwrap();
        let Body::Grid(grid) = &view.pages[0].body else {
            panic!()
        };
        assert!(grid.unasked_facets().is_some());
        assert!(matches!(grid.facets, Facets::Unasked));

        // On the next frame a dispatched facets request is marked Asked
        // and can finish independently of the pending search response.
        let seq = view
            .begin_request(1, 3, &Load::Facets(first_facets))
            .unwrap();
        let Body::Grid(grid) = &view.pages[0].body else {
            panic!()
        };
        assert!(grid.unasked_facets().is_none());
        assert!(matches!(grid.facets, Facets::Asked));
        view.loaded(seq, Ok(Loaded::Facets(Ok(CatalogFacets::default()))));
        assert!(view.pages[0].loading);
        assert!(
            matches!(&view.pages[0].body, Body::Grid(grid) if matches!(grid.facets, Facets::Got(_)))
        );
    }

    #[test]
    fn a_shuffle_pages_through_its_own_order() {
        let mut j = CatalogView::new(1);
        let mut g = Grid::new("Movies".into(), CatalogQuery::default());
        g.query.sort = CatalogSort::Random;
        j.pages[0].body = Body::Grid(Box::new(g));
        let order: Vec<String> = (0..150).map(|i| format!("id{i}")).collect();
        let items = |ids: &[String]| CatalogPage {
            items: ids.iter().map(|i| item(i, CatalogKind::Movie)).collect(),
            total: 0,
        };
        j.loaded(
            1,
            Ok(Loaded::Grid {
                page: items(&order[..60]),
                append: false,
                order: Some(order.clone()),
                by_id: true,
            }),
        );
        let Body::Grid(g) = &j.pages[0].body else {
            panic!()
        };
        assert_eq!(g.total, 150, "the shuffle's length");
        // The next page is the order's next 60, by id.
        let next = g.next_page();
        assert_eq!((next.start, next.ids.len()), (60, 60));
        assert_eq!(next.ids[0], "id60");
        j.loaded(
            1,
            Ok(Loaded::Grid {
                page: items(&next.ids),
                append: true,
                order: None,
                by_id: true,
            }),
        );
        // A reload (after watching something) keeps the order.
        let Some((_, Load::Grid(q))) = j.reload() else {
            panic!()
        };
        assert_eq!(q.ids.len(), 120);
        assert_eq!(q.ids[..3], order[..3]);
        j.loaded(
            1,
            Ok(Loaded::Grid {
                page: items(&q.ids),
                append: false,
                order: None,
                by_id: true,
            }),
        );
        let Body::Grid(g) = &j.pages[0].body else {
            panic!()
        };
        assert_eq!((g.items.len(), g.total), (120, 150));
        assert!(g.order.is_some());
        // The drawer's choices don't end the page's loading.
        j.pages[0].loading = true;
        j.loaded(1, Ok(Loaded::Facets(Ok(CatalogFacets::default()))));
        assert!(j.pages[0].loading);
    }

    #[test]
    fn filters_are_chips_until_taken_off() {
        let mut g = Grid::new("Tagged sea".into(), CatalogQuery::tagged("sea"));
        assert!(g.labels().is_empty(), "the page's own tag is no filter");
        g.query.genres = vec!["Drama".into()];
        g.query.years = (1990..2000).chain([2004]).collect();
        g.query.min_width = Some(3840);
        g.query.tags.push("night".into());
        assert_eq!(
            g.labels(),
            ["Genre · Drama", "1990s", "4K and up", "Tag · night"],
            "a decade counts whole"
        );
        g.take_off(1);
        assert_eq!(g.query.years, [2004]);
        g.clear_filters();
        assert!(g.labels().is_empty());
        assert_eq!(g.query.tags, ["sea"]);
        assert_eq!(decades(&[2021, 1999, 2020, 1985]), [2020, 1990, 1980]);
    }

    #[test]
    fn marks_show_everywhere() {
        let mut j = CatalogView::new(1);
        let mut watched = item("e1", CatalogKind::Episode);
        watched.position = 300.0;
        j.loaded(
            1,
            Ok(Loaded::Home(CatalogHome {
                resume: vec![watched.clone()],
                ..CatalogHome::default()
            })),
        );
        j.pages.push(Page {
            seq: 2,
            request_seq: None,
            loading: false,
            error: None,
            body: Body::Item {
                item: Box::new(watched),
            },
        });
        j.mark("e1", Mark::Played, true);
        j.mark("e1", Mark::Favorite, true);
        let Body::Item { item } = &j.pages[1].body else {
            panic!()
        };
        assert!(item.played && item.favorite && item.position == 0.0);
        let Body::Home { home: Some(h), .. } = &j.pages[0].body else {
            panic!()
        };
        assert!(h.resume[0].played);
    }

    #[test]
    fn search_pages_show_rows_by_kind() {
        let mut j = CatalogView::new(1);
        j.pages.push(Page {
            seq: 2,
            request_seq: None,
            loading: true,
            error: None,
            body: Body::Search {
                search: "alien".into(),
                typed: None,
                asked: "alien".into(),
                results: vec![],
            },
        });
        assert!(!has_content(&j.pages[1].body));
        j.loaded(
            2,
            Ok(Loaded::Search {
                text: "aliens".into(),
                results: vec![
                    ("Movies", vec![item("m1", CatalogKind::Movie)]),
                    ("Shows", vec![item("s1", CatalogKind::Series)]),
                ],
            }),
        );
        let Body::Search { asked, results, .. } = &j.pages[1].body else {
            panic!()
        };
        assert_eq!(asked, "aliens");
        assert_eq!(results.len(), 2);
        assert_eq!(trail(&j.pages), "Search \"aliens\"");

        // Marks show in the results; a reload searches again.
        j.mark("s1", Mark::Favorite, true);
        let Body::Search { results, .. } = &j.pages[1].body else {
            panic!()
        };
        assert!(results[1].1[0].favorite);
        let (seq, load) = j.reload().unwrap();
        assert_eq!(seq, 2);
        assert!(matches!(load, Load::Search(t) if t == "aliens"));
    }

    #[test]
    fn requests_resume_from_the_server() {
        let mut m = item("m1", CatalogKind::Movie);
        m.location = "mb:http://jf#item=m1".into();
        assert_eq!(request("jf", &m, false).start_at, None);
        m.position = 120.0;
        assert_eq!(request("jf", &m, false).start_at, Some(120.0));
        assert_eq!(request("jf", &m, true).start_at, Some(0.0));
        let r = request("jf", &m, false);
        assert_eq!(r.source_id.as_deref(), Some("jf"));
        assert_eq!(r.entry.unwrap().location, "mb:http://jf#item=m1");
        assert_eq!(clip("one two three four", 9), "one two…");
        assert_eq!(clip("short", 9), "short");
    }
}
