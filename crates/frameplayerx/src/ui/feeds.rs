//! DeoVR feeds and HereSphere APIs: their scene groups as chips over a grid
//! of scenes, searched (in the group chosen) across titles, studios,
//! performers and tags, sorted, shuffled and filtered (studio, people, tags,
//! length, format, rating), with a details page per scene (description,
//! performers, tags, chapters to start from).
//!
//! A HereSphere library lists scene addresses only: scenes on screen have
//! their details fetched as they are needed. A DeoVR listing has no studio,
//! people or tags: while searching or filtering, every scene of the group
//! has its details fetched.

use super::page::{self, Card};
use super::theme::{self, Weight};
use super::widgets::{self, Kind, ListArea, Tip};
use super::{Action, FormatFilter, View, fmt_date, fmt_time, icons};
use crate::playback::OpenRequest;
use crate::shuffle::{key as shuffle_key, new_seed};
use egui::{RichText, Sense, Vec2};
use fp_sources::{FeedLibrary, FeedScene, SceneInfo, SourceKind};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Scenes shown at first, and more each time the end of the grid shows.
const PAGE: usize = 60;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FeedSort {
    /// As the feed lists them.
    #[default]
    Feed,
    Title,
    /// By release (or addition) date.
    Newest,
    Rating,
    Length,
    /// Shuffled by [`FeedView::seed`].
    Shuffle,
}

impl FeedSort {
    /// Whether this order reads best largest first.
    fn default_descending(self) -> bool {
        matches!(self, FeedSort::Newest | FeedSort::Rating | FeedSort::Length)
    }
}

/// How long a scene runs, as the Filters drawer offers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Length {
    Short,
    Medium,
    Long,
    Longest,
}

impl Length {
    const ALL: [Length; 4] = [Length::Short, Length::Medium, Length::Long, Length::Longest];

    fn label(self) -> &'static str {
        match self {
            Length::Short => tr!("feed-length-short"),
            Length::Medium => tr!("feed-length-medium"),
            Length::Long => tr!("feed-length-long"),
            Length::Longest => tr!("feed-length-longest"),
        }
    }

    fn holds(self, seconds: f64) -> bool {
        let minutes = seconds / 60.0;
        match self {
            Length::Short => minutes < 10.0,
            Length::Medium => (10.0..30.0).contains(&minutes),
            Length::Long => (30.0..60.0).contains(&minutes),
            Length::Longest => minutes >= 60.0,
        }
    }
}

/// The lowest ratings the drawer offers, in tenths of a star.
fn ratings() -> [(u8, &'static str); 3] {
    [
        (30, tr!("feed-rating-3")),
        (40, tr!("feed-rating-4")),
        (45, tr!("feed-rating-45")),
    ]
}

/// The shapes the drawer offers, as the library's format chips name them.
fn shapes() -> [(FormatFilter, &'static str); 4] {
    [
        (FormatFilter::Flat, tr!("filter-flat")),
        (FormatFilter::Vr180, "180°"),
        (FormatFilter::Vr360, "360°"),
        (FormatFilter::Fisheye, tr!("filter-fisheye")),
    ]
}

/// A scene's shape, as the library's format chips name them.
fn shape(s: &FeedScene) -> Option<FormatFilter> {
    use fp_library::ProjectionKind as P;
    Some(match P::of(&s.format?.projection) {
        P::Flat => FormatFilter::Flat,
        P::Equirect180 => FormatFilter::Vr180,
        P::Equirect360 | P::Eac => FormatFilter::Vr360,
        P::Fisheye => FormatFilter::Fisheye,
    })
}

/// What the Filters drawer keeps: scenes with any of the studios, people,
/// tags and shapes chosen (each kind narrowing the others), of the length
/// chosen and rated at least as high.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeedFilters {
    pub studios: BTreeSet<String>,
    pub people: BTreeSet<String>,
    pub tags: BTreeSet<String>,
    pub shapes: Vec<FormatFilter>,
    /// 3D only.
    pub stereo: bool,
    pub length: Option<Length>,
    /// Tenths of a star (see [`ratings`]).
    pub rating: Option<u8>,
}

/// One filter that is on, as its chip under the sorts names it.
enum On {
    Studio(String),
    Person(String),
    Tag(String),
    Shape(FormatFilter),
    Stereo,
    Length(Length),
    Rating(u8),
}

impl FeedFilters {
    fn on(&self) -> Vec<On> {
        let mut on: Vec<On> = self.studios.iter().cloned().map(On::Studio).collect();
        on.extend(self.people.iter().cloned().map(On::Person));
        on.extend(self.tags.iter().cloned().map(On::Tag));
        on.extend(self.shapes.iter().copied().map(On::Shape));
        if self.stereo {
            on.push(On::Stereo);
        }
        on.extend(self.length.map(On::Length));
        on.extend(self.rating.map(On::Rating));
        on
    }

    pub fn count(&self) -> usize {
        self.on().len()
    }

    /// The chips of the filters that are on.
    fn labels(&self) -> Vec<String> {
        self.on()
            .into_iter()
            .map(|o| match o {
                On::Studio(s) => tr!("filter-on-studio", studio = s),
                On::Person(p) => p,
                On::Tag(t) => tr!("filter-on-tag", tag = t),
                On::Shape(f) => shape_label(f).to_string(),
                On::Stereo => "3D".to_string(),
                On::Length(l) => l.label().to_string(),
                On::Rating(r) => rating_label(r).to_string(),
            })
            .collect()
    }

    /// Takes off the filter of chip `i` (see [`FeedFilters::labels`]).
    fn take_off(&mut self, i: usize) {
        match self.on().into_iter().nth(i) {
            Some(On::Studio(s)) => {
                self.studios.remove(&s);
            }
            Some(On::Person(p)) => {
                self.people.remove(&p);
            }
            Some(On::Tag(t)) => {
                self.tags.remove(&t);
            }
            Some(On::Shape(f)) => self.shapes.retain(|x| *x != f),
            Some(On::Stereo) => self.stereo = false,
            Some(On::Length(_)) => self.length = None,
            Some(On::Rating(_)) => self.rating = None,
            None => {}
        }
    }

    fn keeps(&self, s: &FeedScene) -> bool {
        let any = |chosen: &BTreeSet<String>, has: &[String]| {
            chosen.is_empty() || has.iter().any(|x| chosen.contains(x))
        };
        (self.studios.is_empty() || s.studio.as_ref().is_some_and(|x| self.studios.contains(x)))
            && any(&self.people, &s.actors)
            && any(&self.tags, &s.tags)
            && (self.shapes.is_empty() || shape(s).is_some_and(|f| self.shapes.contains(&f)))
            && (!self.stereo || s.format.is_some_and(|f| f.stereo.is_stereo()))
            && self
                .length
                .is_none_or(|l| s.duration.is_some_and(|d| l.holds(d)))
            && self
                .rating
                .is_none_or(|r| s.rating.is_some_and(|x| x * 10.0 + 0.01 >= r as f32))
    }
}

fn shape_label(f: FormatFilter) -> &'static str {
    shapes()
        .iter()
        .find(|(s, _)| *s == f)
        .map_or("", |(_, label)| label)
}

fn rating_label(r: u8) -> &'static str {
    ratings()
        .iter()
        .find(|(x, _)| *x == r)
        .map_or("", |(_, label)| label)
}

/// The drawer's choices among the scenes the page lists before its
/// filters, each with how many have it, most first.
#[derive(Default)]
struct Facets {
    studios: Vec<(String, usize)>,
    people: Vec<(String, usize)>,
    tags: Vec<(String, usize)>,
    shapes: Vec<(FormatFilter, usize)>,
    stereo: usize,
    timed: bool,
    rated: bool,
}

/// How many times each name comes, most first, then by name.
fn tally<'a>(names: impl Iterator<Item = &'a String>) -> Vec<(String, usize)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for n in names {
        *counts.entry(n.as_str()).or_default() += 1;
    }
    let mut out: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(n, c)| (n.to_string(), c))
        .collect();
    out.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    out
}

pub struct FeedView {
    pub kind: SourceKind,
    /// The server's listing is on its way (the one saved last time may be
    /// shown meanwhile).
    pub loading: bool,
    /// A HereSphere scan is on its way: titles, people and tags of every
    /// scene.
    pub scanning: bool,
    pub error: Option<String>,
    /// Groups in feed order with their scenes' URLs.
    pub groups: Vec<(String, Vec<String>)>,
    pub scenes: HashMap<String, FeedScene>,
    /// Every scene once, as first listed.
    pub order: Vec<String>,
    /// The group shown; `None` for all of them.
    pub group: Option<usize>,
    pub search: String,
    pub sort: FeedSort,
    /// Largest first.
    pub descending: bool,
    /// The order of [`FeedSort::Shuffle`]: new each visit and each Shuffle
    /// again, so opening a scene's page and coming back keeps it.
    pub seed: u64,
    pub scripted_only: bool,
    pub favorites_only: bool,
    pub filters: FeedFilters,
    pub filters_open: bool,
    /// How the drawer's long sections are narrowed, by title.
    pub lists: HashMap<&'static str, page::ListState>,
    /// Scenes shown (paging).
    pub shown: usize,
    /// The grid goes back to its top (the drawer changed a filter).
    pub to_top: bool,
    pub details: Option<Details>,
    /// Scenes whose details were asked for.
    pub requested: HashSet<String>,
    /// Scenes whose details came since the listing was last saved.
    pub unsaved: usize,
}

/// A scene's details page.
pub struct Details {
    pub url: String,
    pub info: Option<Box<SceneInfo>>,
    pub loading: bool,
    pub error: Option<String>,
}

impl FeedView {
    pub fn new(kind: SourceKind) -> FeedView {
        FeedView {
            kind,
            loading: true,
            scanning: false,
            error: None,
            groups: Vec::new(),
            scenes: HashMap::new(),
            order: Vec::new(),
            group: None,
            search: String::new(),
            sort: FeedSort::Feed,
            descending: false,
            seed: new_seed(),
            scripted_only: false,
            favorites_only: false,
            filters: FeedFilters::default(),
            filters_open: false,
            lists: HashMap::new(),
            shown: PAGE,
            to_top: false,
            details: None,
            requested: HashSet::new(),
            unsaved: 0,
        }
    }

    /// Takes the feed's groups and scenes, keeping what is known already:
    /// details read, and titles a bare listing doesn't have. `cached`: the
    /// listing saved last time, shown while the server answers (only into
    /// an empty page).
    pub fn loaded(&mut self, result: Result<FeedLibrary, String>, cached: bool) {
        if cached && !self.order.is_empty() {
            return;
        }
        if !cached {
            self.loading = false;
        }
        let lib = match result {
            Ok(l) => l,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        self.error = None;
        // A HereSphere listing is addresses only; the scan follows it.
        if !cached && self.kind == SourceKind::HereSphere {
            self.scanning = true;
        }
        self.order = lib.scenes.iter().map(|s| s.url.clone()).collect();
        let mut scenes = HashMap::with_capacity(lib.scenes.len());
        for s in lib.scenes {
            let keep = self
                .scenes
                .remove(&s.url)
                .filter(|old| old.detailed || (s.bare && !old.bare));
            scenes.insert(s.url.clone(), keep.unwrap_or(s));
        }
        self.scenes = scenes;
        self.groups = lib.groups;
        if self.group.is_some_and(|g| g >= self.groups.len()) {
            self.group = None;
        }
        // One group: show it rather than "All" beside it.
        if self.groups.len() == 1 {
            self.group = Some(0);
        }
    }

    /// Takes a HereSphere scan into the scenes (`None`: the server has
    /// none). Scenes whose own details came first keep them: they say the
    /// same and more.
    pub fn scanned(&mut self, scan: Option<Vec<FeedScene>>) {
        self.scanning = false;
        for found in scan.into_iter().flatten() {
            if let Some(s) = self.scenes.get_mut(&found.url).filter(|s| !s.detailed) {
                s.merge(found);
            }
        }
    }

    /// Shows a favourite at once, on the scene's card and page; the server
    /// is told separately.
    pub fn mark_favorite(&mut self, url: &str, on: bool) {
        if let Some(s) = self.scenes.get_mut(url) {
            s.favorite = on;
        }
        if let Some(info) = self
            .details
            .as_mut()
            .filter(|d| d.url == url)
            .and_then(|d| d.info.as_mut())
        {
            info.favorite = Some(on);
        }
    }

    /// Takes a scene's details: into its card, and its page when open
    /// (`full`: with media, for the page).
    pub fn scene_loaded(&mut self, url: &str, result: Result<SceneInfo, String>, full: bool) {
        match result {
            Ok(info) => {
                if let Some(s) = self.scenes.get_mut(url) {
                    s.apply(&info);
                    self.unsaved += 1;
                }
                if full && let Some(d) = self.details.as_mut().filter(|d| d.url == url) {
                    d.loading = false;
                    d.info = Some(Box::new(info));
                }
            }
            Err(e) => {
                log::debug!("scene details {url}: {e}");
                // A card without details keeps what the listing said.
                if let Some(s) = self.scenes.get_mut(url) {
                    s.detailed = true;
                }
                if full && let Some(d) = self.details.as_mut().filter(|d| d.url == url) {
                    d.loading = false;
                    d.error = Some(e);
                }
            }
        }
    }

    /// URLs of the scenes in the group shown (all of them for "All").
    fn scope(&self) -> &[String] {
        match self.group.and_then(|g| self.groups.get(g)) {
            Some((_, urls)) => urls,
            None => &self.order,
        }
    }

    /// The scenes of the group shown, searched and with the Scripted and
    /// Favorites chips, before the drawer's filters.
    fn searched(&self) -> Vec<&FeedScene> {
        let words: Vec<String> = self
            .search
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        // A search looks through the group chosen ("All": every group).
        self.scope()
            .iter()
            .filter_map(|u| self.scenes.get(u))
            .filter(|s| words.is_empty() || s.matches(&words))
            .filter(|s| !self.scripted_only || s.scripted)
            .filter(|s| !self.favorites_only || s.favorite)
            .collect()
    }

    /// The scenes to show: searched, filtered and sorted.
    pub fn visible(&self) -> Vec<&FeedScene> {
        let mut out: Vec<&FeedScene> = self
            .searched()
            .into_iter()
            .filter(|s| self.filters.keeps(s))
            .collect();
        let title = |s: &FeedScene| widgets::display_title(&s.title).to_lowercase();
        // Known values first in either direction, the rest after in feed
        // order (the sorts are stable).
        use std::cmp::Ordering;
        fn known<T: PartialOrd>(a: Option<T>, b: Option<T>, descending: bool) -> Ordering {
            match (a, b) {
                (Some(a), Some(b)) => {
                    let o = a.partial_cmp(&b).unwrap_or(Ordering::Equal);
                    if descending { o.reverse() } else { o }
                }
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        }
        let date = |s: &FeedScene| s.released.or(s.added);
        let down = self.descending;
        match self.sort {
            FeedSort::Feed => {}
            FeedSort::Title => {
                out.sort_by_cached_key(|s| title(s));
                if down {
                    out.reverse();
                }
            }
            FeedSort::Newest => out.sort_by(|a, b| known(date(a), date(b), down)),
            FeedSort::Rating => out.sort_by(|a, b| known(a.rating, b.rating, down)),
            FeedSort::Length => out.sort_by(|a, b| known(a.duration, b.duration, down)),
            FeedSort::Shuffle => out.sort_by_cached_key(|s| shuffle_key(self.seed, &s.url)),
        }
        out
    }

    /// The drawer's choices, each section's counted over the scenes the
    /// other sections keep: how many picking one would show. What is chosen
    /// stays listed, with nothing left too.
    fn facets(&self) -> Facets {
        let scenes = self.searched();
        let kept_without = |clear: fn(&mut FeedFilters)| -> Vec<&FeedScene> {
            let mut others = self.filters.clone();
            clear(&mut others);
            scenes.iter().copied().filter(|s| others.keeps(s)).collect()
        };
        let chosen_too = |mut counts: Vec<(String, usize)>, chosen: &BTreeSet<String>| {
            for c in chosen {
                if !counts.iter().any(|(n, _)| n == c) {
                    counts.push((c.clone(), 0));
                }
            }
            counts
        };
        let by_studio = kept_without(|f| f.studios.clear());
        let by_person = kept_without(|f| f.people.clear());
        let by_tag = kept_without(|f| f.tags.clear());
        let by_shape = kept_without(|f| f.shapes.clear());
        let by_stereo = kept_without(|f| f.stereo = false);
        let mut shapes: Vec<(FormatFilter, usize)> = shapes()
            .iter()
            .map(|(f, _)| (*f, by_shape.iter().filter(|s| shape(s) == Some(*f)).count()))
            .filter(|(f, n)| *n > 0 || self.filters.shapes.contains(f))
            .collect();
        shapes.sort_by_key(|s| std::cmp::Reverse(s.1));
        Facets {
            studios: chosen_too(
                tally(by_studio.iter().filter_map(|s| s.studio.as_ref())),
                &self.filters.studios,
            ),
            people: chosen_too(
                tally(by_person.iter().flat_map(|s| s.actors.iter())),
                &self.filters.people,
            ),
            tags: chosen_too(
                tally(by_tag.iter().flat_map(|s| s.tags.iter())),
                &self.filters.tags,
            ),
            shapes,
            stereo: by_stereo
                .iter()
                .filter(|s| s.format.is_some_and(|f| f.stereo.is_stereo()))
                .count(),
            timed: scenes.iter().any(|s| s.duration.is_some()),
            rated: scenes.iter().any(|s| s.rating.is_some()),
        }
    }

    /// Of the group's scenes that need their details read for their studio,
    /// people and tags, how many have them, of how many; `None` once all do.
    fn reading(&self) -> Option<(usize, usize)> {
        let deovr = self.kind == SourceKind::DeoVr;
        let scope = self.scope();
        let pending = scope
            .iter()
            .filter(|u| {
                self.scenes
                    .get(*u)
                    .is_some_and(|s| !s.detailed && (s.bare || deovr))
            })
            .count();
        (pending > 0).then(|| (scope.len() - pending, scope.len()))
    }

    /// Whether any scene says `has`.
    fn any(&self, has: impl Fn(&FeedScene) -> bool) -> bool {
        self.scenes.values().any(has)
    }

    /// Scenes of the group shown that need their details fetched to be
    /// searched, not asked for yet: those known by their address only (no
    /// scan said more), and a DeoVR feed's, whose listing has no tags or
    /// performers.
    fn unfilled(&self) -> Vec<String> {
        let deovr = self.kind == SourceKind::DeoVr;
        self.scope()
            .iter()
            .filter(|u| !self.requested.contains(*u))
            .filter(|u| {
                self.scenes
                    .get(*u)
                    .is_some_and(|s| !s.detailed && (s.bare || deovr))
            })
            .cloned()
            .collect()
    }

    /// The groups and scenes as shown, details included, to save as the
    /// listing shown next time.
    pub fn library(&self) -> FeedLibrary {
        FeedLibrary {
            groups: self.groups.clone(),
            scenes: self
                .order
                .iter()
                .filter_map(|u| self.scenes.get(u).cloned())
                .collect(),
        }
    }
}

/// Opens `scene`.
/// Opens a scene whose page has every detail: opening needs no second fetch.
fn scene_request(
    source: &str,
    scene: &FeedScene,
    info: Option<&SceneInfo>,
    start_at: Option<f64>,
) -> OpenRequest {
    let mut req = request(source, scene, start_at);
    if let Some(info) = info {
        let mut e = scene.to_entry();
        info.apply_to(&mut e);
        req.entry = Some(e);
    }
    req
}

fn request(source: &str, scene: &FeedScene, start_at: Option<f64>) -> OpenRequest {
    OpenRequest {
        location: scene.url.clone(),
        source_id: Some(source.to_string()),
        entry: Some(scene.to_entry()),
        title: None,
        start_at,
        ..Default::default()
    }
}

/// "25:12 · VRStudio" or "25:12 · 12 Oct 2026".
fn card_facts(s: &FeedScene) -> String {
    let mut f = Vec::new();
    if let Some(d) = s.duration {
        f.push(fmt_time(d));
    }
    if let Some(studio) = &s.studio {
        f.push(studio.clone());
    } else if let Some(t) = s.released.or(s.added) {
        f.push(fmt_date(t));
    }
    f.join("  ·  ")
}

/// A feed's scenes or a scene's page; true when Back was clicked.
pub fn page(ui: &mut egui::Ui, v: &mut View, source: &str, name: &str, f: &mut FeedView) -> bool {
    if f.details.is_some() {
        return details(ui, v, source, name, f);
    }
    let haptics = v.settings.haptics_enabled;
    if !haptics {
        f.scripted_only = false;
    }
    let back = header(ui, v.actions, source, name, f);
    let mut changed = filters(ui, f, haptics);
    let count = tr!("count-scenes", count = f.visible().len());
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let choices = sort_choices(f);
        if page::sort_bar(ui, &choices, &mut f.sort) {
            f.descending = f.sort.default_descending();
            f.seed = new_seed();
            changed = true;
        }
        match f.sort {
            FeedSort::Feed => {}
            FeedSort::Shuffle => {
                if page::shuffle_again(ui) {
                    f.seed = new_seed();
                    changed = true;
                }
            }
            sort => changed |= page::direction(ui, &mut f.descending, direction_tips(sort)),
        }
        page::count_label(ui, &count);
    });
    match page::active_filters(ui, &f.filters.labels()) {
        Some(page::FilterEdit::Remove(i)) => {
            f.filters.take_off(i);
            changed = true;
        }
        Some(page::FilterEdit::Clear) => {
            f.filters = FeedFilters::default();
            changed = true;
        }
        None => {}
    }
    ui.add_space(10.0);
    page::status(ui, f.loading && f.order.is_empty(), f.error.as_deref());

    let shown = f.visible();
    let total = shown.len();
    let list: Vec<FeedScene> = shown.into_iter().take(f.shown).cloned().collect();
    let top = std::mem::take(&mut f.to_top) || changed;
    let picked = scene_grid(ui, v.thumbs, f, &list, total, haptics, top);
    if f.filters_open {
        let facets = f.facets();
        let mut open = true;
        let count = tr!("count-scenes", count = total);
        let clear = page::filter_drawer(ui, &mut open, &count, |ui| {
            changed |= drawer(ui, f, &facets);
        });
        f.filters_open = open;
        if clear {
            f.filters = FeedFilters::default();
            changed = true;
        }
    }
    if changed {
        f.shown = PAGE;
        f.to_top = true;
    }
    if picked.more {
        f.shown += PAGE;
    }
    if let Some(i) = picked.play {
        let reqs = list.iter().map(|s| request(source, s, None)).collect();
        v.actions.push(Action::OpenList(reqs, i));
    }
    if let Some(url) = picked.open {
        f.details = Some(Details {
            url: url.clone(),
            info: None,
            loading: true,
            error: None,
        });
        v.actions.push(Action::FeedScene {
            source: source.to_string(),
            url,
        });
    }
    // A DeoVR listing has what a card shows; a HereSphere one, addresses.
    let on_screen = if f.kind == SourceKind::HereSphere {
        picked.on_screen
    } else {
        Vec::new()
    };
    fill_details(v.actions, source, f, on_screen);
    back
}

/// The Filters drawer's sections, those the scenes have choices for; true
/// when a filter changed.
fn drawer(ui: &mut egui::Ui, f: &mut FeedView, facets: &Facets) -> bool {
    let mut changed = false;
    if let Some((done, of)) = f.reading() {
        ui.label(
            RichText::new(tr!("feed-reading-details", done = done, of = of)).color(theme::TEXT_2),
        );
        ui.add(egui::ProgressBar::new(done as f32 / of.max(1) as f32).desired_height(6.0));
    } else if f.scanning {
        ui.label(RichText::new(tr!("feed-reading")).color(theme::TEXT_2));
    }
    let sections = [
        (
            tr!("filter-studio"),
            tr!("filter-find-studio"),
            &facets.studios,
            &mut f.filters.studios,
        ),
        (
            tr!("filter-people"),
            tr!("filter-find-person"),
            &facets.people,
            &mut f.filters.people,
        ),
        (
            tr!("filter-tags"),
            tr!("filter-find-tag"),
            &facets.tags,
            &mut f.filters.tags,
        ),
    ];
    for (title, hint, items, chosen) in sections {
        if items.is_empty() {
            continue;
        }
        let choices: Vec<(&str, Option<usize>)> =
            items.iter().map(|(n, c)| (n.as_str(), Some(*c))).collect();
        let state = f.lists.entry(title).or_default();
        if let Some(name) =
            page::filter_list(ui, title, hint, &choices, |n| chosen.contains(n), state)
        {
            if !chosen.remove(name) {
                chosen.insert(name.to_string());
            }
            changed = true;
        }
    }
    if facets.timed {
        page::filter_section(ui, tr!("filter-length"), |ui| {
            for (l, label) in Length::ALL.map(|l| (l, l.label())) {
                let on = f.filters.length == Some(l);
                if page::filter_chip(ui, label, None, on) {
                    f.filters.length = (!on).then_some(l);
                    changed = true;
                }
            }
        });
    }
    if !facets.shapes.is_empty() || facets.stereo > 0 {
        page::filter_section(ui, tr!("adjust-format"), |ui| {
            for (s, n) in &facets.shapes {
                let on = f.filters.shapes.contains(s);
                if page::filter_chip(ui, shape_label(*s), Some(*n), on) {
                    if on {
                        f.filters.shapes.retain(|x| x != s);
                    } else {
                        f.filters.shapes.push(*s);
                    }
                    changed = true;
                }
            }
            if facets.stereo > 0
                && page::filter_chip(ui, "3D", Some(facets.stereo), f.filters.stereo)
            {
                f.filters.stereo = !f.filters.stereo;
                changed = true;
            }
        });
    }
    if facets.rated {
        page::filter_section(ui, tr!("filter-rating"), |ui| {
            for (r, label) in ratings() {
                let on = f.filters.rating == Some(r);
                if page::filter_chip(ui, label, None, on) {
                    f.filters.rating = (!on).then_some(r);
                    changed = true;
                }
            }
        });
    }
    changed
}

/// What the direction caret says for `sort`: largest first, smallest first.
fn direction_tips(sort: FeedSort) -> [&'static str; 2] {
    match sort {
        FeedSort::Title => [tr!("sort-z-to-a"), tr!("sort-a-to-z")],
        FeedSort::Newest => [tr!("sort-newest-first"), tr!("sort-oldest-first")],
        FeedSort::Rating => [tr!("sort-highest-first"), tr!("sort-lowest-first")],
        _ => [tr!("sort-longest-first"), tr!("sort-shortest-first")],
    }
}

/// The page's title, how many scenes, and Refresh; true when Back was
/// clicked.
fn header(
    ui: &mut egui::Ui,
    actions: &mut Vec<Action>,
    source: &str,
    name: &str,
    f: &mut FeedView,
) -> bool {
    let mut subtitle = format!(
        "{} · {}",
        super::sources::kind_label(f.kind),
        tr!("count-scenes", count = f.order.len())
    );
    // Scenes are shown (last time's, or a listing before its scan) while
    // the rest arrives.
    if !f.order.is_empty() && (f.loading || f.scanning) {
        subtitle.push_str(&format!(" · {}", tr!("feed-updating")));
    }
    page::header(ui, name, Some(&subtitle), |ui| {
        if widgets::icon_button(ui, icons::ARROWS_CLOCKWISE, 44.0, false)
            .tip(tr!("feed-refresh"))
            .clicked()
        {
            f.loading = true;
            actions.push(Action::FeedLoad {
                source: source.to_string(),
            });
        }
    })
}

/// Search, the Scripted and Favorites filters, and the groups; true when
/// they changed.
fn filters(ui: &mut egui::Ui, f: &mut FeedView, haptics: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        changed |= page::search_field(ui, &mut f.search, tr!("feed-search"), 300.0);
        if page::filters_button(ui, f.filters.count()).clicked() {
            f.filters_open = !f.filters_open;
        }
        ui.add_space(10.0);
        if haptics
            && f.any(|s| s.scripted)
            && widgets::chip_icon(
                ui,
                Some(icons::WAVEFORM),
                tr!("feed-scripted"),
                f.scripted_only,
            )
            .clicked()
        {
            f.scripted_only = !f.scripted_only;
            changed = true;
        }
        if f.any(|s| s.favorite)
            && widgets::chip_icon(
                ui,
                Some(icons::HEART),
                tr!("filter-favorites"),
                f.favorites_only,
            )
            .clicked()
        {
            f.favorites_only = !f.favorites_only;
            changed = true;
        }
    });
    if f.groups.len() > 1 {
        ui.add_space(6.0);
        egui::ScrollArea::horizontal()
            .id_salt("feed-groups")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show_rail(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if widgets::chip(ui, tr!("filter-all"), f.group.is_none()).clicked() {
                        f.group = None;
                        changed = true;
                    }
                    for (i, (g, urls)) in f.groups.iter().enumerate() {
                        let label = format!("{g}  {}", urls.len());
                        if widgets::chip(ui, &label, f.group == Some(i)).clicked() {
                            f.group = Some(i);
                            changed = true;
                        }
                    }
                });
            });
    }
    changed
}

/// The orders the scenes can be shown in: those the feed has data for.
fn sort_choices(f: &FeedView) -> Vec<(FeedSort, &'static str)> {
    let mut sorts = vec![
        (FeedSort::Feed, tr!("sort-feed")),
        (FeedSort::Title, tr!("sort-title")),
    ];
    if f.any(|s| s.released.or(s.added).is_some()) {
        sorts.push((FeedSort::Newest, tr!("sort-newest")));
    }
    if f.any(|s| s.rating.is_some()) {
        sorts.push((FeedSort::Rating, tr!("filter-rating")));
    }
    sorts.extend([
        (FeedSort::Length, tr!("filter-length")),
        (FeedSort::Shuffle, tr!("sort-shuffle")),
    ]);
    sorts
}

/// What was picked in the grid of scenes.
#[derive(Default)]
struct Picked {
    /// Play scene `.0` of the list.
    play: Option<usize>,
    /// Open a scene's page.
    open: Option<String>,
    more: bool,
    /// Scenes on screen still without their details.
    on_screen: Vec<String>,
}

fn scene_grid(
    ui: &mut egui::Ui,
    thumbs: &mut super::thumbs::Thumbs,
    f: &FeedView,
    list: &[FeedScene],
    total: usize,
    haptics: bool,
    top: bool,
) -> Picked {
    let mut picked = Picked::default();
    page::scroll_from(ui, top, |ui| {
        if list.is_empty() && !f.loading && f.error.is_none() {
            let (title, body) = if f.order.is_empty() {
                (tr!("feed-no-scenes"), tr!("feed-no-scenes-desc"))
            } else {
                (
                    tr!("filter-nothing-matches"),
                    tr!("filter-nothing-matches-desc"),
                )
            };
            page::empty_state(ui, icons::VIRTUAL_REALITY, title, body, |_| {});
        }
        page::grid(ui, list.len(), page::LANDSCAPE.x, |ui, i| {
            let s = &list[i];
            let title = widgets::display_title(&s.title);
            let facts = card_facts(s);
            let badge = s.format.map(|f| widgets::format_short(&f));
            let c = page::card(
                ui,
                thumbs,
                &Card {
                    thumb: s.thumbnail_url.as_deref(),
                    placeholder: icons::VIRTUAL_REALITY,
                    title: &title,
                    facts: &facts,
                    badge: badge.as_deref(),
                    corner: (haptics && s.scripted).then_some(icons::WAVEFORM),
                    favorite: s.favorite,
                    more: true,
                    ..Card::default()
                },
            );
            if c.visible && !s.detailed {
                picked.on_screen.push(s.url.clone());
            }
            if c.open {
                picked.play = Some(i);
            }
            if c.more {
                picked.open = Some(s.url.clone());
            }
        });
        picked.more = total > list.len() && page::load_more(ui, false);
    });
    picked
}

/// Asks for the details of scenes whose listing lacks them: cards on screen
/// first; while searching or filtering, the rest of the group after, so
/// their studios, performers and tags are searched and filtered too.
fn fill_details(
    actions: &mut Vec<Action>,
    source: &str,
    f: &mut FeedView,
    mut on_screen: Vec<String>,
) {
    on_screen.retain(|u| f.requested.insert(u.clone()));
    if !on_screen.is_empty() {
        actions.push(Action::FeedFill {
            source: source.to_string(),
            urls: on_screen,
            front: true,
        });
    }
    // A scan on its way brings what a search looks through anyway.
    if (!f.search.trim().is_empty() || f.filters_open) && !f.scanning {
        let rest = f.unfilled();
        if !rest.is_empty() {
            f.requested.extend(rest.iter().cloned());
            actions.push(Action::FeedFill {
                source: source.to_string(),
                urls: rest,
                front: false,
            });
        }
    }
}

/// A scene's page; true when Back was clicked.
fn details(ui: &mut egui::Ui, v: &mut View, source: &str, name: &str, f: &mut FeedView) -> bool {
    let Some(d) = f.details.as_ref() else {
        return false;
    };
    let mut scene = f.scenes.get(&d.url).cloned().unwrap_or_else(|| FeedScene {
        url: d.url.clone(),
        ..FeedScene::default()
    });
    if let Some(info) = &d.info {
        scene.apply(info);
    }
    let title = widgets::display_title(&scene.title);
    let back = page::header(ui, name, Some(&title), |_| {});
    page::status(ui, d.loading, d.error.as_deref());
    let info = d.info.clone();
    // The server says whether it takes favourites from players.
    let writable = info.as_ref().is_some_and(|i| i.write_favorite);
    let (mut play_at, mut search) = (None::<f64>, None::<String>);
    let mut favorite = None::<bool>;
    let queued = scene_request(source, &scene, info.as_deref(), None);
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show_list(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 28.0;
                let (rect, _) = ui.allocate_exact_size(Vec2::new(512.0, 288.0), Sense::hover());
                widgets::shadow(ui.painter(), rect, 16, 0.6);
                page::picture(
                    ui,
                    v.thumbs,
                    scene.thumbnail_url.as_deref(),
                    rect,
                    16,
                    icons::VIRTUAL_REALITY,
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.label(
                        RichText::new(&title)
                            .font(theme::font(Weight::Bold, 34.0))
                            .color(theme::TEXT),
                    );
                    let mut facts = Vec::new();
                    if let Some(t) = scene.duration {
                        facts.push(fmt_time(t));
                    }
                    if let Some(t) = scene.released {
                        facts.push(fmt_date(t));
                    }
                    if let Some(fm) = scene.format {
                        facts.push(widgets::format_short(&fm));
                    }
                    if let Some(r) = scene.rating {
                        facts.push(page::rating(r));
                    }
                    if scene.favorite && !writable {
                        facts.push(format!("{}  {}", icons::HEART, tr!("feed-favorite")));
                    }
                    if scene.scripted && v.settings.haptics_enabled {
                        facts.push(format!("{}  {}", icons::WAVEFORM, tr!("settings-haptics")));
                    }
                    page::facts(ui, &facts);
                    ui.add_space(10.0);
                    if widgets::button(ui, Some(icons::PLAY), tr!("player-play"), Kind::Primary)
                        .clicked()
                    {
                        play_at = Some(0.0);
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        super::queue::buttons(ui, v, &queued);
                    });
                    if writable {
                        let label = if scene.favorite {
                            tr!("feed-favorite")
                        } else {
                            tr!("feed-add-favorite")
                        };
                        if widgets::chip_icon(ui, Some(icons::HEART), label, scene.favorite)
                            .clicked()
                        {
                            favorite = Some(!scene.favorite);
                        }
                    }
                    if let Some(info) = &info
                        && !info.sources.is_empty()
                    {
                        let mut versions: Vec<String> = info
                            .sources
                            .iter()
                            .map(|s| match (s.height, s.label.is_empty()) {
                                (Some(h), false) => format!("{h}p {}", s.label),
                                (Some(h), true) => format!("{h}p"),
                                (None, _) => s.label.clone(),
                            })
                            .filter(|s| !s.is_empty())
                            .collect();
                        versions.dedup();
                        if !versions.is_empty() {
                            ui.label(
                                RichText::new(tr!("feed-versions", versions = versions.join(", ")))
                                    .font(theme::font(Weight::Regular, 15.0))
                                    .color(theme::TEXT_3),
                            );
                        }
                    }
                });
            });
            ui.add_space(22.0);
            ui.set_max_width(1100.0);
            if let Some(text) = info.as_ref().and_then(|i| i.description.as_deref()) {
                page::prose(ui, text);
                ui.add_space(16.0);
            }
            if !scene.actors.is_empty() {
                widgets::section_label(ui, tr!("feed-performers"));
                search = search.take().or(page::pills(ui, &scene.actors));
                ui.add_space(14.0);
            }
            if !scene.tags.is_empty() {
                widgets::section_label(ui, tr!("filter-tags"));
                search = search.take().or(page::pills(ui, &scene.tags));
                ui.add_space(14.0);
            }
            let markers = info.as_ref().map(|i| i.markers.as_slice()).unwrap_or(&[]);
            if !markers.is_empty() {
                widgets::section_label(ui, tr!("adjust-chapters"));
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    for (t, name) in markers {
                        let label = if name.is_empty() {
                            fmt_time(*t)
                        } else {
                            format!("{}  {name}", fmt_time(*t))
                        };
                        if widgets::chip_icon(ui, Some(icons::PLAY), &label, false)
                            .tip(tr!("feed-play-from-here"))
                            .clicked()
                        {
                            play_at = Some(*t);
                        }
                    }
                });
            }
            ui.add_space(24.0);
        });
    if let Some(t) = play_at {
        let req = scene_request(source, &scene, info.as_deref(), Some(t));
        v.actions.push(Action::Open(req));
    }
    if let Some(on) = favorite {
        f.mark_favorite(&scene.url, on);
        v.actions.push(Action::FeedFavorite {
            source: source.to_string(),
            url: scene.url.clone(),
            on,
        });
    }
    if let Some(s) = search {
        // A performer or tag: the scenes that have it.
        f.search = s;
        f.group = None;
        f.shown = PAGE;
        f.details = None;
    }
    back
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(url: &str, title: &str, duration: f64) -> FeedScene {
        FeedScene {
            url: url.into(),
            title: title.into(),
            duration: Some(duration),
            ..FeedScene::default()
        }
    }

    fn view() -> FeedView {
        let mut f = FeedView::new(SourceKind::DeoVr);
        f.loaded(
            Ok(FeedLibrary {
                groups: vec![
                    ("Recent".into(), vec!["a".into(), "b".into()]),
                    ("Scripted".into(), vec!["c".into()]),
                ],
                scenes: vec![
                    scene("a", "Beach", 60.0),
                    FeedScene {
                        studio: Some("Lake Films".into()),
                        scripted: true,
                        ..scene("b", "Alps", 600.0)
                    },
                    FeedScene {
                        actors: vec!["Jane".into()],
                        rating: Some(4.5),
                        ..scene("c", "Canyon", 30.0)
                    },
                ],
            }),
            false,
        );
        f
    }

    fn titles(f: &FeedView) -> Vec<String> {
        f.visible().iter().map(|s| s.title.clone()).collect()
    }

    #[test]
    fn groups_search_filters_and_orders() {
        let mut f = view();
        assert!(!f.loading && f.group.is_none());
        assert_eq!(titles(&f), ["Beach", "Alps", "Canyon"]);
        f.group = Some(1);
        assert_eq!(titles(&f), ["Canyon"]);
        // A search looks through the group chosen, studios and performers
        // too; "All" looks through every group.
        f.search = "lake".into();
        assert!(titles(&f).is_empty());
        f.search = "JANE".into();
        assert_eq!(titles(&f), ["Canyon"]);
        f.group = None;
        f.search = "lake".into();
        assert_eq!(titles(&f), ["Alps"]);
        f.search.clear();
        f.sort = FeedSort::Title;
        assert_eq!(titles(&f), ["Alps", "Beach", "Canyon"]);
        f.descending = true;
        assert_eq!(titles(&f), ["Canyon", "Beach", "Alps"]);
        f.sort = FeedSort::Length;
        assert_eq!(titles(&f), ["Alps", "Beach", "Canyon"]);
        f.descending = false;
        assert_eq!(titles(&f), ["Canyon", "Beach", "Alps"]);
        f.descending = true;
        f.sort = FeedSort::Rating;
        assert_eq!(titles(&f)[0], "Canyon");
        f.sort = FeedSort::Feed;
        f.scripted_only = true;
        assert_eq!(titles(&f), ["Alps"]);
    }

    /// Ten scenes: studios, people, tags, shapes, lengths and ratings to
    /// filter by.
    fn varied() -> FeedView {
        use fp_core::format::{Projection, StereoLayout, VideoFormat};
        let vr180 = VideoFormat::new(Projection::EQUIRECT_180, StereoLayout::SideBySide);
        let flat = VideoFormat::new(Projection::Flat, StereoLayout::Mono);
        let scenes: Vec<FeedScene> = (0..10)
            .map(|i| FeedScene {
                studio: Some(if i < 6 { "North" } else { "South" }.into()),
                actors: if i % 2 == 0 {
                    vec!["Ana".into()]
                } else {
                    vec!["Bo".into(), "Ana".into()]
                },
                tags: if i < 3 {
                    vec!["beach".into()]
                } else {
                    vec!["night".into()]
                },
                format: Some(if i < 7 { vr180 } else { flat }),
                rating: Some(i as f32 / 2.0),
                ..scene(
                    &format!("s{i}"),
                    &format!("Scene {i}"),
                    (i * 10 * 60) as f64,
                )
            })
            .collect();
        let mut f = FeedView::new(SourceKind::HereSphere);
        f.loaded(
            Ok(FeedLibrary {
                groups: vec![("All".into(), scenes.iter().map(|s| s.url.clone()).collect())],
                scenes,
            }),
            false,
        );
        f
    }

    #[test]
    fn the_drawer_narrows_and_counts() {
        let mut f = varied();
        let facets = f.facets();
        assert_eq!(facets.studios, [("North".into(), 6), ("South".into(), 4)]);
        assert_eq!(facets.people, [("Ana".into(), 10), ("Bo".into(), 5)]);
        assert_eq!(
            facets.shapes,
            [(FormatFilter::Vr180, 7), (FormatFilter::Flat, 3)]
        );
        assert_eq!(facets.stereo, 7);
        // Any of a kind, all of the kinds.
        f.filters.studios.insert("North".into());
        assert_eq!(f.visible().len(), 6);
        f.filters.tags.insert("beach".into());
        f.filters.tags.insert("night".into());
        assert_eq!(f.visible().len(), 6);
        f.filters.people.insert("Bo".into());
        assert_eq!(titles(&f), ["Scene 1", "Scene 3", "Scene 5"]);
        f.filters.length = Some(Length::Medium);
        assert_eq!(titles(&f), ["Scene 1"], "10 min; 30 is not under 30");
        f.filters.length = Some(Length::Long);
        assert_eq!(titles(&f), ["Scene 3", "Scene 5"]);
        f.filters.rating = Some(20);
        assert_eq!(titles(&f), ["Scene 5"], "rated 2.5; Scene 3 1.5");
        f.filters.rating = Some(30);
        // Each on as a chip, taken off one by one.
        assert_eq!(
            f.filters.labels(),
            [
                "Studio · North",
                "Bo",
                "Tag · beach",
                "Tag · night",
                "30–60 min",
                "3★ and up"
            ]
        );
        f.filters.take_off(5);
        f.filters.take_off(1);
        assert_eq!(f.filters.count(), 4);
        assert!(f.filters.people.is_empty() && f.filters.rating.is_none());
        // Each section counts within what the others keep: North's scenes of
        // 30 to 60 minutes. South has none there, and is left out.
        let facets = f.facets();
        assert_eq!(facets.studios, [("North".into(), 3)]);
        assert_eq!(facets.people, [("Ana".into(), 3), ("Bo".into(), 2)]);
        f.filters.studios = ["South".to_string()].into();
        assert_eq!(
            f.facets().studios,
            [("North".into(), 3), ("South".into(), 0)],
            "a choice stays listed with nothing left"
        );
        f.filters = FeedFilters {
            shapes: vec![FormatFilter::Flat],
            ..FeedFilters::default()
        };
        assert_eq!(f.visible().len(), 3);
        f.filters.stereo = true;
        assert!(f.visible().is_empty(), "the flat scenes are 2D");
    }

    #[test]
    fn a_shuffle_holds_until_shuffled_again() {
        let mut f = varied();
        f.sort = FeedSort::Shuffle;
        f.seed = 1;
        let first = titles(&f);
        assert_eq!(first.len(), 10);
        assert_eq!(titles(&f), first, "the same seed, the same order");
        f.seed = 2;
        let second = titles(&f);
        assert_ne!(second, first);
        let mut sorted = second.clone();
        sorted.sort();
        assert_eq!(sorted.len(), 10, "every scene once");
        assert!(FeedSort::Length.default_descending() && !FeedSort::Title.default_descending());
    }

    #[test]
    fn filtering_a_deovr_feed_reads_every_scene() {
        // Its listing has no studios, people or tags.
        let mut f = view();
        let mut actions = Vec::new();
        fill_details(&mut actions, "s", &mut f, Vec::new());
        assert!(actions.is_empty(), "nothing asked for while just browsing");
        assert_eq!(f.reading(), Some((0, 3)));
        f.filters_open = true;
        fill_details(&mut actions, "s", &mut f, Vec::new());
        match actions.as_slice() {
            [Action::FeedFill { urls, front, .. }] => {
                assert_eq!(urls, &["a", "b", "c"]);
                assert!(!front);
            }
            other => panic!("{other:?}"),
        }
        f.scene_loaded("b", Ok(SceneInfo::default()), false);
        assert_eq!(f.reading(), Some((1, 3)));
    }

    #[test]
    fn favourites_show_at_once() {
        let mut f = view();
        f.details = Some(Details {
            url: "a".into(),
            info: Some(Box::new(SceneInfo {
                favorite: Some(false),
                write_favorite: true,
                ..SceneInfo::default()
            })),
            loading: false,
            error: None,
        });
        f.mark_favorite("a", true);
        assert!(f.scenes["a"].favorite);
        let d = f.details.as_ref().unwrap();
        assert_eq!(d.info.as_ref().unwrap().favorite, Some(true));
        f.favorites_only = true;
        assert_eq!(titles(&f), ["Beach"]);
    }

    #[test]
    fn a_deovr_search_fetches_details_and_they_are_kept() {
        // A DeoVR listing has titles but no tags: a search fetches the rest.
        let mut f = view();
        assert_eq!(f.unfilled(), ["a", "b", "c"]);
        let info = SceneInfo {
            tags: vec!["Sunset".into()],
            ..SceneInfo::default()
        };
        f.scene_loaded("a", Ok(info), false);
        assert_eq!((f.unfilled(), f.unsaved), (vec!["b".into(), "c".into()], 1));
        f.search = "sunset".into();
        assert_eq!(titles(&f), ["Beach"]);
        let saved = f.library();
        assert!(saved.scenes[0].detailed && saved.scenes[0].tags == ["Sunset"]);
        assert_eq!(saved.groups, f.groups);
        // A HereSphere scene a scan described needs no fetching.
        let mut h = view();
        h.kind = SourceKind::HereSphere;
        assert!(h.unfilled().is_empty());
    }

    #[test]
    fn details_fill_cards_and_survive_a_refresh() {
        let mut f = view();
        // Known by their address only, as a HereSphere listing without a
        // scan has them.
        for s in f.scenes.values_mut() {
            s.bare = true;
        }
        f.details = Some(Details {
            url: "a".into(),
            info: None,
            loading: true,
            error: None,
        });
        let info = SceneInfo {
            title: "Beach Day".into(),
            scripts: vec!["s.funscript".into()],
            ..SceneInfo::default()
        };
        f.scene_loaded("a", Ok(info), true);
        assert!(f.details.as_ref().unwrap().info.is_some());
        assert!(f.scenes["a"].detailed && f.scenes["a"].scripted);
        // A failed fetch is not retried, and the card keeps its listing.
        f.scene_loaded("b", Err("timeout".into()), false);
        assert!(f.scenes["b"].detailed);
        assert_eq!(f.scenes["b"].title, "Alps");
        assert_eq!(f.unfilled(), ["c"]);
        // A search in a group fetches that group's scenes only.
        f.group = Some(0);
        assert!(f.unfilled().is_empty());
        f.group = None;

        f.loaded(
            Ok(FeedLibrary {
                groups: vec![("Recent".into(), vec!["a".into()])],
                scenes: vec![scene("a", "Beach", 60.0)],
            }),
            false,
        );
        assert_eq!(f.scenes["a"].title, "Beach Day");
        assert_eq!(f.group, Some(0), "a lone group is shown as such");
    }

    #[test]
    fn last_listing_first_then_the_servers_then_its_scan() {
        let bare = |url: &str| FeedScene {
            bare: true,
            ..scene(url, url, 0.0)
        };
        let listing = |scenes: Vec<FeedScene>| FeedLibrary {
            groups: vec![(
                "Recent".into(),
                scenes.iter().map(|s| s.url.clone()).collect(),
            )],
            scenes,
        };
        let mut f = FeedView::new(SourceKind::HereSphere);
        // Last time's listing, with the titles its scan gave.
        f.loaded(Ok(listing(vec![scene("12", "Beach", 60.0)])), true);
        assert!(f.loading && !f.scanning && titles(&f) == ["Beach"]);
        // Not over a page that shows something already (a refresh).
        f.loaded(Ok(listing(vec![scene("99", "Old", 1.0)])), true);
        assert_eq!(titles(&f), ["Beach"]);

        // The server's bare listing keeps the titles it doesn't have; a new
        // scene shows its address until the scan.
        f.loaded(Ok(listing(vec![bare("12"), bare("13")])), false);
        assert!(!f.loading && f.scanning);
        assert_eq!(titles(&f), ["Beach", "13"]);
        // Bare scenes are what a search would fetch (the page waits for the
        // scan instead while it is on its way).
        f.search = "13".into();
        assert_eq!(f.unfilled(), ["13"]);
        // A card whose own details came first keeps them.
        if let Some(s) = f.scenes.get_mut("12") {
            s.detailed = true;
            s.duration = Some(2627.0);
        }
        f.scanned(Some(vec![
            FeedScene {
                tags: vec!["Sun".into()],
                ..scene("13", "Lake", 90.0)
            },
            scene("12", "Beach", 2.6),
        ]));
        assert!(!f.scanning && !f.scenes["13"].bare);
        assert_eq!(f.scenes["12"].duration, Some(2627.0));
        assert!(f.unfilled().is_empty(), "the scan answers the search");
        f.search = "sun".into();
        assert_eq!(titles(&f), ["Lake"]);
    }
}
