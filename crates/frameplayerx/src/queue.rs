//! Up next: the videos playing in turn. Opening a video from a list (a
//! folder, a grid, a season) makes that list the queue for X/Y; Play all, a
//! playlist and what you queue yourself also play through, the next video
//! starting when one ends.

use crate::playback::OpenRequest;

#[derive(Default)]
pub struct Queue {
    pub items: Vec<OpenRequest>,
    /// The one playing (or last played).
    pub pos: usize,
    /// The next one starts when one ends.
    pub plays_through: bool,
    /// Counts changes.
    pub version: u64,
}

impl Queue {
    /// The list a video was opened from, at `pos`.
    pub fn set(&mut self, items: Vec<OpenRequest>, pos: usize, plays_through: bool) {
        self.pos = pos.min(items.len().saturating_sub(1));
        self.items = items;
        self.plays_through = plays_through;
        self.version += 1;
    }

    pub fn clear(&mut self) {
        self.set(Vec::new(), 0, false);
    }

    /// Plays the one at `i` next: it becomes the current.
    pub fn jump(&mut self, i: usize) -> Option<&OpenRequest> {
        let req = self.items.get(i)?;
        self.pos = i;
        self.version += 1;
        Some(req)
    }

    /// The one `dir` steps from the current; past either end, round to
    /// the other when the list `loops`.
    pub fn neighbour(&self, dir: i32, loops: bool) -> Option<usize> {
        let n = self.items.len() as i32;
        let i = self.pos as i32 + dir;
        if (0..n).contains(&i) {
            Some(i as usize)
        } else {
            (loops && n > 1).then(|| i.rem_euclid(n) as usize)
        }
    }

    /// The one that starts when the current ends, when the list plays
    /// through (or `autoplay` makes it): the next, or the first again after
    /// the last when it `loops` (the current itself when it is the only
    /// one).
    pub fn up_next(&self, autoplay: bool, loops: bool) -> Option<usize> {
        if !(self.plays_through || autoplay) || self.items.is_empty() {
            return None;
        }
        let next = self.pos + 1;
        if next < self.items.len() {
            Some(next)
        } else {
            loops.then_some(0)
        }
    }

    /// Queues `req` right after the one playing (`next`) or at the end; a
    /// video opened alone (`playing`) joins the empty queue first. What is
    /// queued by hand plays through.
    pub fn add(&mut self, req: OpenRequest, playing: &OpenRequest, next: bool) {
        if self.items.is_empty() {
            self.items.push(playing.clone());
            self.pos = 0;
        }
        let at = if next { self.pos + 1 } else { self.items.len() };
        self.items.insert(at, req);
        self.plays_through = true;
        self.version += 1;
    }

    /// Takes out the one at `i`, unless it is the one playing.
    pub fn remove(&mut self, i: usize) {
        if i >= self.items.len() || i == self.pos {
            return;
        }
        self.items.remove(i);
        if i < self.pos {
            self.pos -= 1;
        }
        self.version += 1;
    }

    /// Moves the one at `from` to `to`; the one playing stays the current.
    pub fn move_item(&mut self, from: usize, to: usize) {
        if from >= self.items.len() || to >= self.items.len() || from == to {
            return;
        }
        let item = self.items.remove(from);
        self.items.insert(to, item);
        self.pos = if from == self.pos {
            to
        } else if from < self.pos && to >= self.pos {
            self.pos - 1
        } else if from > self.pos && to <= self.pos {
            self.pos + 1
        } else {
            self.pos
        };
        self.version += 1;
    }

    /// Keeps the current video first and shuffles every other entry, using
    /// the same seeded order as feeds. Playback and play-through stay as
    /// they are; another seed gives a new order.
    pub fn shuffle(&mut self, seed: u64) {
        if self.items.len() < 2 {
            return;
        }
        let current = self.items.remove(self.pos);
        self.items
            .sort_by_cached_key(|r| crate::shuffle::key(seed, &r.location));
        self.items.insert(0, current);
        self.pos = 0;
        self.version += 1;
    }

    /// Keeps only the one playing.
    pub fn clear_rest(&mut self) {
        if let Some(current) = self.items.get(self.pos).cloned() {
            self.set(vec![current], 0, self.plays_through);
        } else {
            self.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(name: &str) -> OpenRequest {
        OpenRequest {
            location: format!("/v/{name}.mp4"),
            source_id: None,
            entry: None,
            title: None,
            start_at: None,
            ..Default::default()
        }
    }

    fn names(q: &Queue) -> Vec<String> {
        q.items
            .iter()
            .map(|r| {
                r.location
                    .trim_start_matches("/v/")
                    .trim_end_matches(".mp4")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_list_plays_through_only_when_asked() {
        let mut q = Queue::default();
        q.set(vec![req("a"), req("b")], 0, false);
        assert!(
            q.up_next(false, false).is_none(),
            "opened from a folder: stops at the end"
        );
        assert!(
            q.up_next(false, true).is_none(),
            "looping doesn't make it play through"
        );
        assert_eq!(q.up_next(true, false), Some(1), "autoplay does");
        q.set(vec![req("a"), req("b")], 0, true);
        assert_eq!(q.up_next(false, false), Some(1));
        q.pos = 1;
        assert!(q.up_next(false, false).is_none(), "the last one");
        assert_eq!(q.up_next(false, true), Some(0), "looping: the first again");
        q.set(vec![req("a")], 0, true);
        assert_eq!(q.up_next(false, true), Some(0), "the only one, again");
        q.clear();
        assert!(q.up_next(true, true).is_none());
    }

    #[test]
    fn stepping_goes_round_when_the_list_loops() {
        let mut q = Queue::default();
        q.set(vec![req("a"), req("b"), req("c")], 0, false);
        assert_eq!(q.neighbour(1, false), Some(1));
        assert_eq!(q.neighbour(-1, false), None);
        assert_eq!(q.neighbour(-1, true), Some(2));
        q.pos = 2;
        assert_eq!(q.neighbour(1, false), None);
        assert_eq!(q.neighbour(1, true), Some(0));
        q.set(vec![req("a")], 0, false);
        assert_eq!(q.neighbour(1, true), None, "no other video to step to");
    }

    #[test]
    fn queuing_by_hand() {
        let mut q = Queue::default();
        // The video open now joins first.
        q.add(req("b"), &req("a"), false);
        assert_eq!(
            (names(&q), q.pos, q.plays_through),
            (vec!["a".into(), "b".into()], 0, true)
        );
        q.add(req("c"), &req("a"), true);
        assert_eq!(names(&q), ["a", "c", "b"]);
    }

    #[test]
    fn editing_keeps_the_one_playing() {
        let mut q = Queue::default();
        q.set(["a", "b", "c", "d"].map(req).to_vec(), 1, true);
        q.remove(1);
        assert_eq!(names(&q), ["a", "b", "c", "d"], "not the one playing");
        q.remove(0);
        assert_eq!(
            (names(&q), q.pos),
            (vec!["b".into(), "c".into(), "d".into()], 0)
        );
        q.move_item(2, 0);
        assert_eq!(
            (names(&q), q.pos),
            (vec!["d".into(), "b".into(), "c".into()], 1)
        );
        q.move_item(1, 2);
        assert_eq!(
            (names(&q), q.pos),
            (vec!["d".into(), "c".into(), "b".into()], 2)
        );
        q.move_item(0, 2);
        assert_eq!(
            (names(&q), q.pos),
            (vec!["c".into(), "b".into(), "d".into()], 1)
        );
        q.clear_rest();
        assert_eq!((names(&q), q.pos), (vec!["b".into()], 0));
    }

    #[test]
    fn shuffling_keeps_every_entry_with_the_current_one_first() {
        let items: Vec<_> = (0..12).map(|i| req(&i.to_string())).collect();
        for pos in [0, 6, 11] {
            for plays_through in [false, true] {
                let mut q = Queue::default();
                q.set(items.clone(), pos, plays_through);
                let current = q.items[pos].location.clone();
                let mut before = names(&q);
                let version = q.version;

                q.shuffle(7);

                assert_eq!(q.pos, 0);
                assert_eq!(q.items[q.pos].location, current);
                assert_eq!(q.plays_through, plays_through);
                assert_eq!(q.version, version + 1, "refresh the cached rows");
                let mut after = names(&q);
                before.sort();
                after.sort();
                assert_eq!(after, before, "each queued entry stays exactly once");
            }
        }
    }

    #[test]
    fn a_shuffle_is_stable_for_its_seed_and_can_be_shuffled_again() {
        let items: Vec<_> = (0..20).map(|i| req(&i.to_string())).collect();
        let mut q = Queue::default();
        q.set(items.clone(), 8, true);
        let mut original_tail = names(&q);
        original_tail.remove(8);
        q.shuffle(7);
        let first = names(&q);
        assert_ne!(first[1..], original_tail, "the other entries change order");

        q.shuffle(7);
        assert_eq!(names(&q), first, "the same seed keeps the same order");

        // A location's shuffle key does not depend on the previous order.
        let mut reversed = items;
        reversed.reverse();
        q.set(reversed, 11, true);
        q.shuffle(7);
        assert_eq!(names(&q), first);

        q.shuffle(9);
        assert_eq!(q.items[q.pos].location, "/v/8.mp4");
        assert_ne!(names(&q), first, "a fresh seed gives a new order");
    }

    #[test]
    fn shuffling_preserves_distinct_requests_for_the_same_location() {
        use crate::playback::Quality;
        use fp_core::source::{Entry, EntryKind};

        let items: Vec<_> = ["before", "playing", "after"]
            .into_iter()
            .enumerate()
            .map(|(i, title)| OpenRequest {
                source_id: Some(format!("source-{i}")),
                entry: Some(Entry::new(title, "/v/same.mp4", EntryKind::Video)),
                title: Some(title.into()),
                start_at: Some(i as f64 * 10.0),
                quality: Quality::Transcode(format!("quality-{i}")),
                transcode: Some(format!("target-{i}")),
                ..req("same")
            })
            .collect();
        let mut q = Queue::default();
        q.set(items.clone(), 1, true);

        q.shuffle(7);

        assert_eq!(q.items.len(), items.len());
        assert_eq!(q.pos, 0);
        assert_eq!(q.items[q.pos].title.as_deref(), Some("playing"));
        for expected in &items {
            let actual = q
                .items
                .iter()
                .find(|r| r.title == expected.title)
                .expect("each distinct request remains");
            assert_eq!(actual.location, expected.location);
            assert_eq!(actual.source_id, expected.source_id);
            assert_eq!(actual.entry, expected.entry);
            assert_eq!(actual.start_at, expected.start_at);
            assert_eq!(actual.quality, expected.quality);
            assert_eq!(actual.transcode, expected.transcode);
        }
    }

    #[test]
    fn shuffling_an_empty_or_single_entry_queue_changes_nothing() {
        let mut q = Queue::default();
        q.shuffle(7);
        assert!(q.items.is_empty());
        assert_eq!((q.pos, q.version, q.plays_through), (0, 0, false));

        for plays_through in [false, true] {
            q.set(vec![req("only")], 0, plays_through);
            let version = q.version;
            q.shuffle(7);
            assert_eq!(names(&q), ["only"]);
            assert_eq!(
                (q.pos, q.version, q.plays_through),
                (0, version, plays_through)
            );
        }
    }

    #[test]
    fn shuffling_two_entries_keeps_the_other_one_next() {
        for (pos, expected) in [(0, ["a", "b"]), (1, ["b", "a"])] {
            let mut q = Queue::default();
            q.set(["a", "b"].map(req).to_vec(), pos, true);

            q.shuffle(7);

            assert_eq!(q.pos, 0);
            assert_eq!(names(&q), expected);
            assert_eq!(q.up_next(false, false), Some(1));
        }
    }

    #[test]
    fn a_shuffled_queue_plays_and_loops_in_its_new_order() {
        for plays_through in [false, true] {
            let mut q = Queue::default();
            q.set(
                ["a", "b", "c", "d", "e"].map(req).to_vec(),
                3,
                plays_through,
            );
            q.shuffle(7);
            let order = names(&q);
            assert_eq!(order[0], "d");
            assert_eq!(q.up_next(false, true), plays_through.then_some(1));
            assert_eq!(q.neighbour(-1, false), None);
            assert_eq!(q.neighbour(-1, true), Some(4));

            for (i, expected) in order.iter().enumerate().skip(1) {
                assert_eq!(q.up_next(true, false), Some(i));
                assert_eq!(q.neighbour(1, false), Some(i));
                q.jump(i).expect("the next shuffled entry");
                assert_eq!(&names(&q)[q.pos], expected);
            }
            assert_eq!(q.up_next(true, false), None);
            assert_eq!(q.up_next(true, true), Some(0));
            assert_eq!(q.neighbour(1, true), Some(0));
        }
    }

    #[test]
    fn a_shuffled_queue_can_still_be_edited() {
        let mut q = Queue::default();
        q.set(["a", "b", "c", "d"].map(req).to_vec(), 2, true);
        q.shuffle(7);
        q.add(req("next"), &req("c"), true);
        assert_eq!(names(&q)[1], "next");
        assert_eq!(q.up_next(false, false), Some(1));

        q.move_item(0, 2);
        q.remove(0);
        assert_eq!(q.items[q.pos].location, "/v/c.mp4");
        q.clear_rest();
        assert_eq!((names(&q), q.pos), (vec!["c".into()], 0));
    }
}
