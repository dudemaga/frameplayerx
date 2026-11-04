//! Block cache with background read-ahead, shared by the HTTP and SMB
//! readers.
//!
//! The demuxer reads 32-256 KiB at a time, mostly sequentially, with
//! occasional seeks (often to the end of an MP4 for its `moov` atom). Each
//! network round trip is expensive, so reads are served from fixed-size
//! blocks (1 MiB by default) kept in a small LRU. When access is sequential
//! the next block is fetched on a background thread so playback never waits
//! on the network for data it was always going to need.
//!
//! One block at a time spends most of a slow-responding server's time on
//! round trips, so a fetcher that can stream a range gets one growing request
//! once the reader has kept going in order. The body is cut into blocks as it
//! arrives, and its socket is left unread while the cache is full, which slows
//! the server through TCP and keeps memory at the size of the cache.

use fp_core::ByteSource;
use fp_core::sync::lock;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};

/// Blocks read in order before requests start to grow. Opening a video jumps
/// around, and a probe or thumbnail stops early, so those stay one block at a
/// time.
const GROW_AFTER: u64 = 4;
/// Blocks left behind the reader when a long request fills the cache: a
/// demuxer sometimes steps back a little.
const KEEP_BEHIND: usize = 2;

/// Fetches byte ranges from the underlying file.
pub(crate) trait RangeFetch: Send + Sync + 'static {
    /// Reads up to `len` bytes at `offset`. Returns fewer bytes only at end
    /// of file, and an empty vector past it.
    fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    /// Total size, when known.
    fn size(&self) -> Option<u64>;
    /// Redacted description for logs.
    fn describe(&self) -> String;
    /// True when ranges must be fetched in increasing order (a server
    /// without range support streamed sequentially): read-ahead is then
    /// disabled because it could skip past data the reader still needs.
    fn sequential_only(&self) -> bool {
        false
    }
    /// True when [`Self::open_range`] works. The size must be known, or a
    /// short body can't be told from the end of the file.
    fn long_ranges(&self) -> bool {
        false
    }
    /// Starts one request for `len` bytes at `offset`, read as they arrive.
    fn open_range(&self, _offset: u64, _len: u64) -> io::Result<RangeBody> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// A request started by [`RangeFetch::open_range`].
pub(crate) struct RangeBody {
    pub reader: Box<dyn Read + Send>,
    /// Bytes the server will send: fewer than asked if it caps its ranges.
    pub len: u64,
}

/// Tuning of the block cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheOptions {
    /// Bytes per block. Default 1 MiB.
    pub block_size: usize,
    /// Blocks kept in memory. Default 8 (8 MiB per open file). At least 3
    /// are always kept so a read-ahead block is not evicted before use.
    pub max_blocks: usize,
    /// Fetch ahead in the background when reads are sequential.
    pub read_ahead: bool,
    /// Most blocks one request may ask for. Default 64.
    pub longest_request: usize,
}

impl Default for CacheOptions {
    fn default() -> Self {
        CacheOptions {
            block_size: 1 << 20,
            max_blocks: 8,
            read_ahead: true,
            longest_request: 64,
        }
    }
}

/// Counters for one open file, mainly for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Blocks fetched for a reader that was waiting.
    pub fetches: u64,
    /// Blocks fetched by the read-ahead thread.
    pub read_aheads: u64,
    /// Requests for more than one block.
    pub long_requests: u64,
    /// Block lookups served from memory (including blocks a reader waited
    /// on while the read-ahead thread was fetching them).
    pub hits: u64,
}

enum Slot {
    Ready { data: Arc<Vec<u8>>, used: u64 },
    Loading,
}

struct State {
    blocks: HashMap<u64, Slot>,
    tick: u64,
    last_block: Option<u64>,
    stats: CacheStats,
    /// Whether the block last asked for carried on from the one before.
    in_order: bool,
    /// Blocks moved through in order since the last jump.
    run: u64,
    /// Counts jumps, so a request can tell it has been left behind.
    jumps: u64,
    /// The block held for read-ahead, released when the reader jumps.
    reserved: Option<u64>,
    /// A long request is running, even one a jump has left behind.
    streaming: bool,
    /// The block the current long request delivers next.
    frontier: Option<u64>,
    /// Most blocks a request may ask for: lowered by servers that cap their
    /// ranges or keep failing long ones.
    longest: u64,
    closing: bool,
}

struct Inner<F: RangeFetch> {
    fetcher: F,
    block_size: u64,
    max_blocks: usize,
    read_ahead: bool,
    state: Mutex<State>,
    loaded: Condvar,
    /// Wakes a long request waiting for the reader to make room.
    wake: Condvar,
    ahead: Mutex<Option<Sender<Job>>>,
}

/// Work for the read-ahead thread.
struct Job {
    start: u64,
    blocks: u64,
    jumps: u64,
}

/// How reading a long request's body ended.
enum LongEnd {
    /// The last block is published and the request's state cleared.
    Finished,
    /// A jump or the source closing ended it.
    Stopped,
    /// It broke off.
    Failed,
    /// It broke off after sitting unread: servers may do that, so it doesn't
    /// count against long requests.
    ClosedWhilePaused,
}

/// Why a long request stops early.
enum Leave {
    LeftBehind,
    Closing,
}

/// A [`ByteSource`] reading through the block cache.
pub(crate) struct CachedSource<F: RangeFetch> {
    inner: Arc<Inner<F>>,
}

impl<F: RangeFetch> CachedSource<F> {
    pub(crate) fn new(fetcher: F, opts: &CacheOptions) -> Self {
        CachedSource {
            inner: Arc::new(Inner {
                fetcher,
                block_size: opts.block_size.max(4096) as u64,
                max_blocks: opts.max_blocks.max(3),
                read_ahead: opts.read_ahead,
                state: Mutex::new(State {
                    blocks: HashMap::new(),
                    tick: 0,
                    last_block: None,
                    stats: CacheStats::default(),
                    in_order: false,
                    run: 0,
                    jumps: 0,
                    reserved: None,
                    streaming: false,
                    frontier: None,
                    longest: opts.longest_request.max(1) as u64,
                    closing: false,
                }),
                loaded: Condvar::new(),
                wake: Condvar::new(),
                ahead: Mutex::new(None),
            }),
        }
    }

    /// Puts an already fetched block in the cache (the HTTP probe request
    /// returns block 0 with the size).
    pub(crate) fn seed(&self, index: u64, data: Vec<u8>) {
        let mut st = lock(&self.inner.state);
        self.inner.insert_ready(&mut st, index, Arc::new(data));
    }

    pub(crate) fn fetcher(&self) -> &F {
        &self.inner.fetcher
    }

    pub(crate) fn stats(&self) -> CacheStats {
        lock(&self.inner.state).stats
    }

    pub(crate) fn block_size(&self) -> u64 {
        self.inner.block_size
    }
}

impl<F: RangeFetch> Drop for CachedSource<F> {
    /// Tells a long request to stop. Nothing here waits for it: a blocked
    /// read ends by itself, at worst after the read timeout.
    fn drop(&mut self) {
        lock(&self.inner.state).closing = true;
        self.inner.wake.notify_all();
    }
}

/// Blocks to ask for after `run` blocks read in order: one at first, then
/// doubling, never more than has been read already.
fn request_blocks(run: u64, longest: u64) -> u64 {
    if run < GROW_AFTER {
        return 1;
    }
    (1u64 << (run - 2).ilog2()).min(longest)
}

/// Blocks ready and not reached yet, up to the block a long request delivers
/// next. Others further on are left over from before a jump.
fn unread(st: &State, frontier: u64) -> usize {
    let reader = st.last_block.unwrap_or(0);
    st.blocks
        .iter()
        .filter(|(i, s)| **i > reader && **i < frontier && matches!(s, Slot::Ready { .. }))
        .count()
}

impl<F: RangeFetch> Inner<F> {
    /// Blocks a long request may keep ahead of the reader.
    fn room(&self) -> usize {
        self.max_blocks - KEEP_BEHIND
    }

    fn insert_ready(&self, st: &mut State, index: u64, data: Arc<Vec<u8>>) {
        st.tick += 1;
        let used = st.tick;
        st.blocks.insert(index, Slot::Ready { data, used });
        let ready = st
            .blocks
            .values()
            .filter(|s| matches!(s, Slot::Ready { .. }))
            .count();
        if ready > self.max_blocks {
            let reader = st.last_block.unwrap_or(0);
            let near = self.max_blocks as u64;
            let victim = st
                .blocks
                .iter()
                .filter(|(i, _)| **i != index)
                .filter_map(|(i, s)| match s {
                    // A block about to be read goes last, however long ago
                    // it landed.
                    Slot::Ready { used, .. } => {
                        Some((*i > reader && *i - reader <= near, *used, *i))
                    }
                    Slot::Loading => None,
                })
                .min()
                .map(|(_, _, i)| i);
            if let Some(v) = victim {
                st.blocks.remove(&v);
            }
        }
    }

    /// Records where the reader went. A jump ends the run and releases what
    /// was held for the old position, before the reader could wait on it.
    fn note_position(&self, st: &mut State, index: u64) {
        let last = st.last_block;
        let in_order = last.is_some_and(|l| {
            index == l || index == l + 1 || st.frontier.is_some_and(|f| index > l && index <= f)
        });
        if in_order {
            st.run += index - last.unwrap_or(index);
        } else {
            st.run = 0;
            st.jumps += 1;
            st.frontier = None;
            self.release_reserved(st);
        }
        st.in_order = in_order;
        st.last_block = Some(index);
        if st.streaming {
            self.wake.notify_all();
        }
    }

    fn release_reserved(&self, st: &mut State) {
        if let Some(r) = st.reserved.take()
            && matches!(st.blocks.get(&r), Some(Slot::Loading))
        {
            st.blocks.remove(&r);
            self.loaded.notify_all();
        }
    }

    /// Returns block `index`, fetching it (or waiting for the read-ahead
    /// thread to finish fetching it) when needed.
    fn block(&self, index: u64) -> io::Result<Arc<Vec<u8>>> {
        let mut st = lock(&self.state);
        self.note_position(&mut st, index);
        loop {
            st.tick += 1;
            let tick = st.tick;
            match st.blocks.get_mut(&index) {
                Some(Slot::Ready { data, used }) => {
                    *used = tick;
                    let data = data.clone();
                    st.stats.hits += 1;
                    return Ok(data);
                }
                Some(Slot::Loading) => {
                    st = self.loaded.wait(st).unwrap_or_else(PoisonError::into_inner);
                }
                None => break,
            }
        }
        st.blocks.insert(index, Slot::Loading);
        st.stats.fetches += 1;
        drop(st);
        self.load(index)
    }

    /// Fetches a block already marked `Loading` and publishes the result.
    fn load(&self, index: u64) -> io::Result<Arc<Vec<u8>>> {
        let result = self
            .fetcher
            .fetch(index * self.block_size, self.block_size as usize);
        let mut st = lock(&self.state);
        if st.reserved == Some(index) {
            st.reserved = None;
        }
        let out = match result {
            Ok(data) => {
                let data = Arc::new(data);
                self.insert_ready(&mut st, index, data.clone());
                Ok(data)
            }
            Err(e) => {
                st.blocks.remove(&index);
                Err(e)
            }
        };
        drop(st);
        self.loaded.notify_all();
        out
    }

    /// What to fetch next for a reader on block `index`, reserved so a reader
    /// that gets there first waits for it instead of fetching it too.
    fn next_job(&self, st: &mut State, index: u64) -> Option<Job> {
        if !st.in_order || st.streaming {
            return None;
        }
        let bs = self.block_size;
        let longest = if self.fetcher.long_ranges() {
            st.longest
        } else {
            1
        };
        let want = request_blocks(st.run, longest);
        let mut next = index + 1;
        if want > 1 {
            while matches!(st.blocks.get(&next), Some(Slot::Ready { .. })) {
                next += 1;
            }
            if unread(st, next) >= self.room() {
                return None;
            }
        }
        let size = self.fetcher.size();
        if size.is_some_and(|s| next * bs >= s) || st.blocks.contains_key(&next) {
            return None;
        }
        let blocks = size.map_or(want, |s| want.min((s - next * bs).div_ceil(bs)));
        st.blocks.insert(next, Slot::Loading);
        st.reserved = Some(next);
        if blocks > 1 {
            st.streaming = true;
            st.frontier = Some(next);
        }
        Some(Job {
            start: next,
            blocks,
            jumps: st.jumps,
        })
    }

    /// Gives up a job that never ran. A jump since then has already released
    /// its block.
    fn abandon(&self, job: &Job) {
        let mut st = lock(&self.state);
        if st.jumps == job.jumps && st.reserved == Some(job.start) {
            self.release_reserved(&mut st);
        }
        if job.blocks > 1 {
            st.streaming = false;
            if st.jumps == job.jumps {
                st.frontier = None;
            }
        }
    }

    /// Fetches one block reserved for read-ahead (see [`Self::next_job`]).
    fn read_ahead(&self, job: Job) {
        {
            let mut st = lock(&self.state);
            if st.jumps != job.jumps {
                return;
            }
            st.stats.read_aheads += 1;
        }
        if let Err(e) = self.load(job.start) {
            log::debug!(
                "read-ahead of block {} failed for {}: {e}",
                job.start,
                self.fetcher.describe()
            );
        }
    }

    /// Runs one long request. If it ends early, whatever is reading next
    /// fetches the next unfetched block and requests carry on from there.
    fn stream_ahead(&self, job: Job) {
        let body = self
            .fetcher
            .open_range(job.start * self.block_size, job.blocks * self.block_size);
        lock(&self.state).stats.long_requests += 1;
        let end = match body {
            Ok(body) => self.read_long(&job, body),
            Err(e) => {
                log::debug!(
                    "long request at block {} failed for {}: {e}",
                    job.start,
                    self.fetcher.describe()
                );
                LongEnd::Failed
            }
        };
        if matches!(end, LongEnd::Finished) {
            return;
        }
        let mut st = lock(&self.state);
        st.streaming = false;
        if st.jumps == job.jumps {
            st.frontier = None;
            self.release_reserved(&mut st);
            if matches!(end, LongEnd::Failed) {
                st.longest = (job.blocks / 2).max(1);
            }
        }
    }

    /// Cuts a long request's body into blocks, publishing each as it lands.
    fn read_long(&self, job: &Job, body: RangeBody) -> LongEnd {
        let bs = self.block_size;
        let RangeBody { mut reader, len } = body;
        let to_eof = self
            .fetcher
            .size()
            .is_some_and(|s| job.start * bs + len >= s);
        let whole = if to_eof { len.div_ceil(bs) } else { len / bs };
        if !to_eof && len < job.blocks * bs {
            let mut st = lock(&self.state);
            st.longest = st.longest.min(whole.max(1));
        }
        if whole == 0 {
            return LongEnd::Failed;
        }
        let last = job.start + whole - 1;
        let mut left = len;
        let mut paused = false;
        for index in job.start..=last {
            if let Some(why) = self.wait_for_room(job, index, &mut paused) {
                return Self::leave(reader, left, bs, why);
            }
            let mut data = vec![0u8; left.min(bs) as usize];
            let mut got = 0;
            while got < data.len() {
                match reader.read(&mut data[got..]) {
                    Ok(0) => return Self::broke_off(paused),
                    Ok(n) => {
                        got += n;
                        left -= n as u64;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        log::debug!(
                            "long request broke off at block {index} for {}: {e}",
                            self.fetcher.describe()
                        );
                        return Self::broke_off(paused);
                    }
                }
                if let Some(why) = self.stop_reason(job) {
                    return Self::leave(reader, left, bs, why);
                }
            }
            self.publish(job, index, last, data);
            paused = false;
        }
        if left == 0 {
            // Reaching the end of the body returns its connection to the pool.
            let _ = reader.read(&mut [0u8; 1]);
        }
        LongEnd::Finished
    }

    fn broke_off(paused: bool) -> LongEnd {
        if paused {
            LongEnd::ClosedWhilePaused
        } else {
            LongEnd::Failed
        }
    }

    /// Ends a request something has stopped. Dropping its body closes its
    /// connection, so one within a block of its end is finished instead and
    /// the connection goes back to the pool.
    fn leave(mut reader: Box<dyn Read + Send>, left: u64, bs: u64, why: Leave) -> LongEnd {
        if matches!(why, Leave::LeftBehind) && left <= bs {
            let _ = io::copy(&mut reader, &mut io::sink());
        }
        LongEnd::Stopped
    }

    fn stop_reason(&self, job: &Job) -> Option<Leave> {
        let st = lock(&self.state);
        if st.closing {
            Some(Leave::Closing)
        } else if st.jumps != job.jumps {
            Some(Leave::LeftBehind)
        } else {
            None
        }
    }

    /// Waits while the blocks ahead of the reader fill the cache, leaving the
    /// socket unread. Returns why to stop, if the request should.
    fn wait_for_room(&self, job: &Job, index: u64, paused: &mut bool) -> Option<Leave> {
        let mut st = lock(&self.state);
        loop {
            if st.closing {
                return Some(Leave::Closing);
            }
            if st.jumps != job.jumps {
                return Some(Leave::LeftBehind);
            }
            if unread(&st, index) < self.room() {
                return None;
            }
            *paused = true;
            st = self.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Publishes a block of a long request and holds the next one. The last
    /// block also ends the request's state, in the same step, so a reader
    /// woken by it can start the next request straight away.
    fn publish(&self, job: &Job, index: u64, last: u64, data: Vec<u8>) {
        let mut st = lock(&self.state);
        st.stats.read_aheads += 1;
        self.insert_ready(&mut st, index, Arc::new(data));
        if index == last {
            st.streaming = false;
        }
        if st.jumps == job.jumps {
            st.reserved = None;
            st.frontier = None;
            if index < last {
                let next = index + 1;
                let state = &mut *st;
                state.frontier = Some(next);
                if let Entry::Vacant(slot) = state.blocks.entry(next) {
                    slot.insert(Slot::Loading);
                    state.reserved = Some(next);
                }
            }
        }
        drop(st);
        self.loaded.notify_all();
    }
}

fn read_ahead_worker<F: RangeFetch>(weak: Weak<Inner<F>>, rx: Receiver<Job>) {
    while let Ok(mut job) = rx.recv() {
        let Some(inner) = weak.upgrade() else { break };
        // After a seek only the newest request matters.
        while let Ok(newer) = rx.try_recv() {
            inner.abandon(&job);
            job = newer;
        }
        if job.blocks > 1 {
            inner.stream_ahead(job);
        } else {
            inner.read_ahead(job);
        }
    }
}

impl<F: RangeFetch> CachedSource<F> {
    /// Starts fetching ahead of block `index` when access looks sequential.
    fn note_access(&self, index: u64) {
        let inner = &self.inner;
        if !inner.read_ahead || inner.fetcher.sequential_only() {
            return;
        }
        let Some(job) = inner.next_job(&mut lock(&inner.state), index) else {
            return;
        };
        let mut ahead = lock(&inner.ahead);
        if ahead.is_none() {
            let (tx, rx) = mpsc::channel();
            let weak = Arc::downgrade(inner);
            match std::thread::Builder::new()
                .name("fp-read-ahead".into())
                .spawn(move || read_ahead_worker(weak, rx))
            {
                Ok(_) => *ahead = Some(tx),
                Err(e) => log::warn!("cannot start read-ahead thread: {e}"),
            }
        }
        let refused = match ahead.as_ref() {
            Some(tx) => tx.send(job).err().map(|e| e.0),
            None => Some(job),
        };
        drop(ahead);
        if let Some(job) = refused {
            inner.abandon(&job);
        }
    }
}

impl<F: RangeFetch> ByteSource for CachedSource<F> {
    fn remote(&self) -> bool {
        true
    }

    fn size(&self) -> Option<u64> {
        self.inner.fetcher.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let size = self.size();
        let bs = self.inner.block_size;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            if size.is_some_and(|s| pos >= s) {
                break;
            }
            let index = pos / bs;
            let block = match self.inner.block(index) {
                Ok(b) => b,
                // Return what we have; the caller will ask again.
                Err(_) if done > 0 => break,
                Err(e) => return Err(e),
            };
            self.note_access(index);
            let within = (pos - index * bs) as usize;
            if within >= block.len() {
                break; // past the end of a short (final) block
            }
            let n = (block.len() - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&block[within..within + n]);
            done += n;
            if (block.len() as u64) < bs {
                break; // a short block is the last one
            }
        }
        Ok(done)
    }

    fn describe(&self) -> String {
        self.inner.fetcher.describe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct Mem {
        data: Vec<u8>,
        calls: Arc<AtomicUsize>,
        fail_at: Option<u64>,
        sequential: bool,
    }

    impl RangeFetch for Mem {
        fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_at == Some(offset) {
                return Err(io::Error::other("boom"));
            }
            let start = (offset as usize).min(self.data.len());
            let end = (start + len).min(self.data.len());
            Ok(self.data[start..end].to_vec())
        }
        fn size(&self) -> Option<u64> {
            Some(self.data.len() as u64)
        }
        fn describe(&self) -> String {
            "mem".into()
        }
        fn sequential_only(&self) -> bool {
            self.sequential
        }
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn source(n: usize, opts: CacheOptions) -> (CachedSource<Mem>, Arc<AtomicUsize>, Vec<u8>) {
        let d = data(n);
        let calls = Arc::new(AtomicUsize::new(0));
        let mem = Mem {
            data: d.clone(),
            calls: calls.clone(),
            fail_at: None,
            sequential: false,
        };
        (CachedSource::new(mem, &opts), calls, d)
    }

    fn opts(block: usize, blocks: usize, ahead: bool) -> CacheOptions {
        CacheOptions {
            block_size: block,
            max_blocks: blocks,
            read_ahead: ahead,
            ..CacheOptions::default()
        }
    }

    #[test]
    fn reads_across_blocks_and_eof() {
        let (s, _, d) = source(10_000, opts(4096, 4, false));
        let mut buf = vec![0u8; 5000];
        assert_eq!(s.read_at(3000, &mut buf).unwrap(), 5000);
        assert_eq!(buf, d[3000..8000]);
        assert_eq!(s.read_at(9000, &mut buf).unwrap(), 1000);
        assert_eq!(buf[..1000], d[9000..]);
        assert_eq!(s.read_at(10_000, &mut buf).unwrap(), 0);
        assert_eq!(s.read_at(50_000, &mut buf).unwrap(), 0);
        assert_eq!(s.read_at(0, &mut []).unwrap(), 0);
    }

    #[test]
    fn caches_blocks_and_evicts_lru() {
        let (s, calls, _) = source(64 * 1024, opts(4096, 3, false));
        let mut buf = [0u8; 100];
        for _ in 0..5 {
            s.read_at(10, &mut buf).unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        s.read_at(5000, &mut buf).unwrap(); // block 1
        s.read_at(9000, &mut buf).unwrap(); // block 2
        s.read_at(10, &mut buf).unwrap(); // block 0 again: hit, now most recent
        s.read_at(13_000, &mut buf).unwrap(); // block 3 evicts block 1
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        s.read_at(10, &mut buf).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        s.read_at(5000, &mut buf).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        let st = s.stats();
        assert_eq!(st.fetches, 5);
        assert!(st.hits >= 6);
    }

    #[test]
    fn sequential_reads_trigger_read_ahead() {
        let (s, _, d) = source(64 * 1024, opts(4096, 8, true));
        let mut buf = vec![0u8; 1024];
        let mut pos = 0u64;
        while pos < 8192 {
            let n = s.read_at(pos, &mut buf).unwrap();
            assert_eq!(buf[..n], d[pos as usize..pos as usize + n]);
            pos += n as u64;
        }
        // Block 2 is fetched in the background; wait for it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.stats().read_aheads == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(s.stats().read_aheads >= 1);
        let before = s.stats().fetches;
        s.read_at(8192, &mut buf).unwrap();
        assert_eq!(s.stats().fetches, before, "block 2 came from read-ahead");
        assert_eq!(buf[..], d[8192..9216]);
    }

    #[test]
    fn a_reader_ahead_of_the_read_ahead_waits_for_it() {
        // Repeated: the reader has to get there before the thread runs.
        for _ in 0..50 {
            let (s, _, d) = source(64 * 1024, opts(4096, 8, true));
            let mut buf = vec![0u8; 2048];
            s.read_at(0, &mut buf).unwrap();
            s.read_at(2048, &mut buf).unwrap();
            s.read_at(4096, &mut buf).unwrap();
            assert_eq!(buf[..], d[4096..6144]);
            let st = s.stats();
            assert_eq!(st.fetches, 1, "only block 0 by a reader: {st:?}");
        }
    }

    #[test]
    fn random_access_does_not_read_ahead() {
        let (s, _, _) = source(64 * 1024, opts(4096, 8, true));
        let mut buf = [0u8; 16];
        for off in [0u64, 40_000, 9000, 60_000, 20_000] {
            s.read_at(off, &mut buf).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(s.stats().read_aheads, 0);
    }

    #[test]
    fn sequential_only_fetchers_never_read_ahead() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mem = Mem {
            data: data(32 * 1024),
            calls: calls.clone(),
            fail_at: None,
            sequential: true,
        };
        let s = CachedSource::new(mem, &opts(4096, 4, true));
        let mut buf = [0u8; 4096];
        for i in 0..4 {
            s.read_at(i * 4096, &mut buf).unwrap();
        }
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(s.stats().read_aheads, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn errors_propagate_and_partial_reads_succeed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mem = Mem {
            data: data(16 * 1024),
            calls,
            fail_at: Some(4096),
            sequential: false,
        };
        let s = CachedSource::new(mem, &opts(4096, 4, false));
        let mut buf = vec![0u8; 6000];
        // First block succeeds, second fails: a short read, not an error.
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 4096);
        assert!(s.read_at(5000, &mut buf).is_err());
    }

    #[test]
    fn seeded_block_is_served_without_fetch() {
        let (s, calls, d) = source(10_000, opts(4096, 4, false));
        s.seed(0, d[..4096].to_vec());
        let mut buf = [0u8; 10];
        s.read_at(5, &mut buf).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(buf, d[5..15]);
    }

    #[test]
    fn concurrent_readers_share_fetches() {
        let (s, calls, d) = source(256 * 1024, opts(16 * 1024, 32, false));
        let s = Arc::new(s);
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let s = s.clone();
                let d = d.clone();
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 3000];
                    for i in 0..50u64 {
                        let off = (i * 5003 + t * 17) % 250_000;
                        let n = s.read_at(off, &mut buf).unwrap();
                        assert_eq!(buf[..n], d[off as usize..off as usize + n]);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // 16 blocks in the file, each fetched exactly once.
        assert!(calls.load(Ordering::SeqCst) <= 16);
    }

    #[test]
    fn drop_stops_read_ahead_thread() {
        let (s, _, _) = source(64 * 1024, opts(4096, 8, true));
        let mut buf = [0u8; 4096];
        s.read_at(0, &mut buf).unwrap();
        s.read_at(4096, &mut buf).unwrap();
        let weak = Arc::downgrade(&s.inner);
        drop(s);
        let deadline = Instant::now() + Duration::from_secs(5);
        while weak.upgrade().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(weak.upgrade().is_none());
    }

    const BLOCK: u64 = 4096;

    /// What a [`Long`] fetcher's requests did.
    struct Wire {
        /// Every request, as (offset, length asked for).
        requests: Mutex<Vec<(u64, u64)>>,
        /// Bytes long bodies have sent.
        sent: AtomicUsize,
        /// Long bodies dropped before their end.
        dropped: AtomicUsize,
        /// A long body sends nothing from this offset until it's raised.
        hold: Mutex<u64>,
        raised: Condvar,
        /// A long body breaks off on reaching this offset, once: with an
        /// error, or by just ending.
        break_at: Mutex<Option<(u64, bool)>>,
    }

    impl Wire {
        fn requests(&self) -> Vec<(u64, u64)> {
            lock(&self.requests).clone()
        }

        fn hold_at(&self, offset: u64) {
            *lock(&self.hold) = offset;
            self.raised.notify_all();
        }
    }

    struct Long {
        data: Arc<Vec<u8>>,
        wire: Arc<Wire>,
        /// Most bytes one long request sends.
        cap: Option<u64>,
        long: bool,
        refuse_long: bool,
    }

    impl RangeFetch for Long {
        fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            lock(&self.wire.requests).push((offset, len as u64));
            let start = (offset as usize).min(self.data.len());
            let end = (start + len).min(self.data.len());
            Ok(self.data[start..end].to_vec())
        }
        fn size(&self) -> Option<u64> {
            Some(self.data.len() as u64)
        }
        fn describe(&self) -> String {
            "long".into()
        }
        fn long_ranges(&self) -> bool {
            self.long
        }
        fn open_range(&self, offset: u64, len: u64) -> io::Result<RangeBody> {
            lock(&self.wire.requests).push((offset, len));
            if self.refuse_long {
                return Err(io::ErrorKind::ConnectionRefused.into());
            }
            let rest = self.data.len() as u64 - offset;
            let len = self.cap.map_or(len, |c| len.min(c)).min(rest);
            Ok(RangeBody {
                reader: Box::new(Body {
                    data: self.data.clone(),
                    pos: offset,
                    end: offset + len,
                    wire: self.wire.clone(),
                }),
                len,
            })
        }
    }

    struct Body {
        data: Arc<Vec<u8>>,
        pos: u64,
        end: u64,
        wire: Arc<Wire>,
    }

    impl Read for Body {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos == self.end {
                return Ok(0);
            }
            let mut hold = lock(&self.wire.hold);
            while self.pos >= *hold {
                hold = self
                    .wire
                    .raised
                    .wait(hold)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            let mut stop = self.end.min(*hold);
            drop(hold);
            let mut breaking = lock(&self.wire.break_at);
            if let Some((at, error)) = *breaking {
                if self.pos >= at {
                    *breaking = None;
                    return if error {
                        Err(io::ErrorKind::ConnectionReset.into())
                    } else {
                        Ok(0)
                    };
                }
                stop = stop.min(at);
            }
            drop(breaking);
            let n = buf.len().min(1024).min((stop - self.pos) as usize);
            buf[..n].copy_from_slice(&self.data[self.pos as usize..][..n]);
            self.pos += n as u64;
            self.wire.sent.fetch_add(n, Ordering::SeqCst);
            Ok(n)
        }
    }

    impl Drop for Body {
        fn drop(&mut self) {
            if self.pos < self.end {
                self.wire.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    type LongParts = (Arc<CachedSource<Long>>, Arc<Wire>, Arc<Vec<u8>>);

    fn long_with(bytes: u64, cap: Option<u64>, long: bool, refuse_long: bool) -> LongParts {
        let d = Arc::new(data(bytes as usize));
        let wire = Arc::new(Wire {
            requests: Mutex::default(),
            sent: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            hold: Mutex::new(u64::MAX),
            raised: Condvar::new(),
            break_at: Mutex::new(None),
        });
        let fetcher = Long {
            data: d.clone(),
            wire: wire.clone(),
            cap,
            long,
            refuse_long,
        };
        let s = CachedSource::new(fetcher, &opts(BLOCK as usize, 8, true));
        (Arc::new(s), wire, d)
    }

    fn long_source(blocks: u64) -> LongParts {
        long_with(blocks * BLOCK, None, true, false)
    }

    /// Reads `from..to` a piece at a time like a demuxer, on a thread of its
    /// own so a read that hangs fails the test.
    fn read_range(s: &Arc<CachedSource<Long>>, from: u64, to: u64) -> Vec<u8> {
        let s = s.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let mut buf = [0u8; 1024];
            while from + (out.len() as u64) < to {
                let want = (to - from - out.len() as u64).min(buf.len() as u64) as usize;
                let n = s
                    .read_at(from + out.len() as u64, &mut buf[..want])
                    .unwrap();
                assert_ne!(n, 0, "end of file before {to}");
                out.extend_from_slice(&buf[..n]);
            }
            let _ = tx.send(out);
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the read is stuck")
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Time for a long request to fill the cache and stop reading.
    fn let_it_settle() {
        std::thread::sleep(Duration::from_millis(60));
    }

    /// Reads on until a long request runs at least `past` blocks beyond
    /// where it would wait for the reader, and returns the next block to read.
    fn read_until_mid_request(s: &Arc<CachedSource<Long>>, wire: &Wire, past: u64) -> u64 {
        let mut next = 30;
        read_range(s, 0, next * BLOCK);
        loop {
            let_it_settle();
            let (offset, len) = *wire.requests().last().unwrap();
            if (offset + len) / BLOCK > next + 6 + past {
                return next;
            }
            read_range(s, next * BLOCK, (next + 1) * BLOCK);
            next += 1;
        }
    }

    #[test]
    fn request_sizes_follow_the_table() {
        for (run, want) in [
            (0, 1),
            (3, 1),
            (4, 2),
            (5, 2),
            (6, 4),
            (9, 4),
            (10, 8),
            (18, 16),
            (34, 32),
            (65, 32),
            (66, 64),
            (1000, 64),
        ] {
            assert_eq!(request_blocks(run, 64), want, "after {run} blocks");
        }
        assert_eq!(request_blocks(1000, 16), 16);
        assert_eq!(request_blocks(1000, 1), 1);
    }

    #[test]
    fn requests_grow_as_reads_go_on_and_restart_after_a_jump() {
        let (s, wire, d) = long_source(300);
        assert!(read_range(&s, 0, 200 * BLOCK) == d[..200 * BLOCK as usize]);
        let requests = wire.requests();
        assert!(requests[..5].iter().all(|r| r.1 == BLOCK), "{requests:?}");
        assert_eq!(requests[5], (5 * BLOCK, 2 * BLOCK), "{requests:?}");
        for pair in requests.windows(2) {
            assert_eq!(
                pair[0].0 + pair[0].1,
                pair[1].0,
                "gap or repeat: {requests:?}"
            );
            assert!(pair[0].1 <= pair[1].1, "shrank: {requests:?}");
        }
        assert!(
            requests.iter().all(|r| r.1 <= r.0.max(BLOCK)),
            "{requests:?}"
        );
        assert_eq!(requests.iter().map(|r| r.1).max(), Some(64 * BLOCK));

        let_it_settle();
        let before = wire.requests().len();
        assert!(
            read_range(&s, 280 * BLOCK, 284 * BLOCK)
                == d[280 * BLOCK as usize..284 * BLOCK as usize]
        );
        let after = wire.requests();
        assert_eq!(after[before], (280 * BLOCK, BLOCK));
        assert!(after[before..].iter().all(|r| r.1 == BLOCK), "{after:?}");
    }

    #[test]
    fn a_waiting_reader_gets_a_block_before_its_request_finishes() {
        let (s, wire, d) = long_source(40);
        wire.hold_at(6 * BLOCK);
        assert!(read_range(&s, 0, 6 * BLOCK) == d[..6 * BLOCK as usize]);
        assert!(wire.requests().contains(&(5 * BLOCK, 2 * BLOCK)));
        assert_eq!(wire.sent.load(Ordering::SeqCst), BLOCK as usize);
        wire.hold_at(u64::MAX);
    }

    #[test]
    fn a_jump_drops_a_long_request_and_never_waits_on_it() {
        let (s, wire, d) = long_source(400);
        let next = read_until_mid_request(&s, &wire, 3);
        assert_eq!(wire.dropped.load(Ordering::SeqCst), 0);
        let far = 390 * BLOCK;
        assert!(read_range(&s, far, far + 100) == d[far as usize..far as usize + 100]);
        wait_until("the request is dropped", || {
            wire.dropped.load(Ordering::SeqCst) == 1
        });
        let back = (next + 6) * BLOCK;
        assert!(read_range(&s, back, back + 100) == d[back as usize..back as usize + 100]);

        // A request stuck waiting for its server doesn't hold the reader.
        let (s, wire, d) = long_source(40);
        wire.hold_at(6 * BLOCK);
        read_range(&s, 0, 6 * BLOCK);
        wait_until("the request is waiting on block 6", || {
            matches!(lock(&s.inner.state).blocks.get(&6), Some(Slot::Loading))
        });
        let far = 30 * BLOCK;
        assert!(read_range(&s, far, far + 100) == d[far as usize..far as usize + 100]);
        let held = 6 * BLOCK;
        assert!(read_range(&s, held, held + 100) == d[held as usize..held as usize + 100]);
        wire.hold_at(u64::MAX);
    }

    #[test]
    fn a_request_nearly_done_finishes_instead_of_dropping() {
        let (s, wire, _) = long_source(400);
        let mut next = read_until_mid_request(&s, &wire, 3);
        let (offset, len) = *wire.requests().last().unwrap();
        let end = (offset + len) / BLOCK;
        // A reader on block r leaves the request waiting on block r + 7.
        while next + 7 < end {
            read_range(&s, next * BLOCK, (next + 1) * BLOCK);
            next += 1;
        }
        let_it_settle();
        let far = 395 * BLOCK;
        read_range(&s, far, far + 100);
        wait_until("the last block is read", || !lock(&s.inner.state).streaming);
        assert_eq!(wire.dropped.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_paused_reader_holds_no_more_than_the_cache_then_resumes() {
        let (s, wire, d) = long_source(120);
        read_range(&s, 0, 30 * BLOCK);
        let_it_settle();
        let sent = wire.sent.load(Ordering::SeqCst) as u64;
        assert!(
            sent <= 36 * BLOCK,
            "{sent} bytes while the reader is on block 29"
        );
        let ready = lock(&s.inner.state)
            .blocks
            .values()
            .filter(|b| matches!(b, Slot::Ready { .. }))
            .count();
        assert!(ready <= 8, "{ready} blocks in memory");
        let_it_settle();
        assert_eq!(
            wire.sent.load(Ordering::SeqCst) as u64,
            sent,
            "kept reading"
        );

        assert!(read_range(&s, 30 * BLOCK, 120 * BLOCK) == d[30 * BLOCK as usize..]);
        let requests = wire.requests();
        for pair in requests.windows(2) {
            assert_eq!(pair[0].0 + pair[0].1, pair[1].0, "refetched: {requests:?}");
        }
    }

    #[test]
    fn a_server_closing_a_paused_request_costs_no_error() {
        for error in [true, false] {
            let (s, wire, d) = long_source(400);
            let next = read_until_mid_request(&s, &wire, 3);
            // The request is waiting on block next + 6.
            *lock(&wire.break_at) = Some(((next + 6) * BLOCK, error));
            assert!(
                read_range(&s, next * BLOCK, 400 * BLOCK) == d[next as usize * BLOCK as usize..]
            );
            assert_eq!(lock(&s.inner.state).longest, 64);
            assert!(wire.requests().last().unwrap().1 > BLOCK);
        }
    }

    #[test]
    fn a_body_failing_midway_resumes_and_every_byte_matches() {
        for error in [true, false] {
            let (s, wire, d) = long_source(120);
            let at = 11 * BLOCK + 100;
            *lock(&wire.break_at) = Some((at, error));
            assert!(
                read_range(&s, 0, 120 * BLOCK) == *d,
                "short data taken as the file's"
            );
            let requests = wire.requests();
            assert!(
                requests.iter().filter(|r| r.0 == 11 * BLOCK).count() == 1,
                "the block that failed is fetched again from its start: {requests:?}"
            );
            assert!(requests.last().unwrap().0 > 11 * BLOCK);
        }
    }

    #[test]
    fn a_fetcher_that_refuses_long_requests_falls_back_to_blocks() {
        let (s, wire, d) = long_with(60 * BLOCK, None, true, true);
        assert!(read_range(&s, 0, 60 * BLOCK) == *d);
        assert_eq!(s.stats().long_requests, 1);
        let requests = wire.requests();
        assert_eq!(requests.iter().filter(|r| r.1 > BLOCK).count(), 1);
    }

    #[test]
    fn a_cap_stops_requests_growing_and_the_file_is_read_in_full() {
        let (s, wire, d) = long_with(100 * BLOCK, Some(3 * BLOCK + 100), true, false);
        assert!(read_range(&s, 0, 100 * BLOCK) == *d);
        let requests = wire.requests();
        let longest = requests.iter().map(|r| r.1).max().unwrap();
        assert!(longest <= 4 * BLOCK, "{requests:?}");
        let last = requests.iter().rev().take(5);
        assert!(last.clone().all(|r| r.1 <= 3 * BLOCK), "{requests:?}");

        // Under one block, long requests can't work: back to single blocks.
        let (s, wire, d) = long_with(100 * BLOCK, Some(1024), true, false);
        assert!(read_range(&s, 0, 100 * BLOCK) == *d);
        assert_eq!(s.stats().long_requests, 1, "{:?}", wire.requests());
    }

    #[test]
    fn a_short_last_block_in_a_long_request_is_the_end_of_the_file() {
        let bytes = 100 * BLOCK + 700;
        let (s, _, d) = long_with(bytes, None, true, false);
        assert!(read_range(&s, 0, bytes) == *d);
        let mut buf = [0u8; 16];
        assert_eq!(s.read_at(bytes, &mut buf).unwrap(), 0);
        assert!(s.stats().long_requests > 0);
    }

    #[test]
    fn two_alternating_readers_stay_one_block_at_a_time() {
        let (s, wire, d) = long_source(400);
        for i in 0..60u64 {
            for base in [0, 300 * BLOCK] {
                let at = base + i * 2048;
                assert!(read_range(&s, at, at + 2048) == d[at as usize..at as usize + 2048]);
            }
        }
        assert!(wire.requests().iter().all(|r| r.1 == BLOCK));
        assert_eq!(s.stats().long_requests, 0);
        assert_eq!(wire.dropped.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_fetcher_without_long_requests_stays_one_block_ahead() {
        let (s, wire, d) = long_with(60 * BLOCK, None, false, false);
        for block in 0..50 {
            let at = block * BLOCK;
            assert!(read_range(&s, at, at + BLOCK) == d[at as usize..(at + BLOCK) as usize]);
            let requested = wire.requests().iter().map(|r| r.0).max().unwrap();
            assert!(requested <= (block + 1) * BLOCK, "ahead of block {block}");
        }
        assert!(wire.requests().iter().all(|r| r.1 == BLOCK));
        assert_eq!(s.stats().long_requests, 0);
    }

    /// Hands back the source itself once `read_range`'s threads have let go.
    fn only_owner(s: Arc<CachedSource<Long>>) -> CachedSource<Long> {
        wait_until("the reader lets go", || Arc::strong_count(&s) == 1);
        Arc::try_unwrap(s).ok().unwrap()
    }

    #[test]
    fn closing_never_waits_for_a_long_request() {
        // Waiting for room: it ends at once.
        let (s, _, _) = long_source(120);
        read_range(&s, 0, 30 * BLOCK);
        let_it_settle();
        let s = only_owner(s);
        let weak = Arc::downgrade(&s.inner);
        drop(s);
        wait_until("the paused request ends", || weak.upgrade().is_none());

        // Blocked on a silent server: closing returns anyway, and the request
        // ends when the socket does.
        let (s, wire, _) = long_source(120);
        wire.hold_at(6 * BLOCK);
        read_range(&s, 0, 6 * BLOCK);
        let s = only_owner(s);
        let weak = Arc::downgrade(&s.inner);
        let t = Instant::now();
        drop(s);
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
        assert!(weak.upgrade().is_some(), "still blocked on its socket");
        wire.hold_at(u64::MAX);
        wait_until("the request ends", || weak.upgrade().is_none());
        assert_eq!(wire.dropped.load(Ordering::SeqCst), 1);
    }
}
