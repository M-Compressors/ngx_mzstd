//! Tests of the body filter against a model of the NGINX buffer API.
//!
//! The model follows the C implementation of every [`Host`] call, but is
//! stricter than NGINX where that turns a logic error into one Miri reports:
//!
//! - freed chain links are deallocated at once instead of being kept for reuse,
//!   so a link used after `ngx_free_chain()` is a use-after-free;
//! - output buffers are uninitialized, as `ngx_create_temp_buf()`'s are, so
//!   sending bytes the encoder never wrote is a read of uninitialized memory;
//! - upstream frees each input buffer and every chain link it passes as soon
//!   as the call returns and the buffer is drained, so holding on to either
//!   is a use-after-free.
//!
//! The thread pool is a queue that tests run on their own schedule: a posted
//! task stays queued until a test runs it and reports its completion, as the
//! NGINX event loop would. Or it is made of real threads ([`Threads`]), so
//! that Miri checks what the tasks share with the filter for data races.

use core::cell::RefCell;
use core::ffi::c_void;
use core::fmt;
use core::ptr;
use std::alloc::{Layout, alloc, dealloc};
use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use mzstd::compressor::parallel::{OwnedParallelTask, SegmentSize, TaskCount};
use mzstd::{DecodeOperation, DecoderStatus, Decompressor, EncoderConfig, InputSize, Level};
use ngx::ffi::{NGX_AGAIN, NGX_ERROR, NGX_OK, ngx_buf_t, ngx_chain_t, ngx_int_t, off_t};

use super::{BodyFilter, BufFlags, Host, Parallel, Settings};
use crate::encoder_pool::RetentionPolicy;

/// The request pool, the client and the connection, as the filter sees them.
struct Model {
    /// Its address tags the zstd module's buffers.
    tag: Box<u8>,
    /// Chain links allocated and not yet freed.
    links: HashSet<*mut ngx_chain_t>,
    /// Buffers and memory owned by the pool until the request ends.
    bufs: Vec<*mut ngx_buf_t>,
    memory: Vec<(*mut u8, Layout)>,
    temp_bufs: usize,

    /// Bytes the client takes per call to the next filter; `None` for all.
    budget: Option<usize>,
    /// Buffers passed to the next filter and not yet sent completely.
    pending: VecDeque<*mut ngx_buf_t>,
    /// What the client received, and where flushes and the end came.
    sent: Vec<u8>,
    flushes: Vec<usize>,
    last: Option<usize>,

    buffered: bool,
    errors: RefCell<Vec<String>>,

    /// Tasks posted to the thread pool and not run yet.
    tasks: VecDeque<OwnedParallelTask>,
    /// Tasks posted in all.
    posted: usize,
    /// The thread pool's queue is full: posting fails.
    overflow: bool,
    /// `r->main->blocked`.
    blocked: bool,
    /// Runs posted tasks instead of `tasks`.
    threads: Option<Threads>,
}

/// A thread pool: its threads take posted tasks, run them and report each
/// completion, which the test hands to the filter as the event loop does.
struct Threads {
    tasks: Sender<OwnedParallelTask>,
    done: Receiver<()>,
    workers: Vec<JoinHandle<()>>,
}

impl Threads {
    fn new(count: usize) -> Self {
        let (tasks, queue) = channel::<OwnedParallelTask>();
        let queue = Arc::new(Mutex::new(queue));
        let (done_tx, done) = channel();
        let workers = (0..count)
            .map(|_| {
                let queue = Arc::clone(&queue);
                let done = done_tx.clone();
                thread::spawn(move || {
                    loop {
                        let task = queue.lock().unwrap().recv();
                        let Ok(task) = task else {
                            return;
                        };
                        task.run();
                        // The test may have finished with the filter already.
                        let _ = done.send(());
                    }
                })
            })
            .collect();
        Self {
            tasks,
            done,
            workers,
        }
    }
}

impl Model {
    fn new(budget: Option<usize>) -> Self {
        Self {
            tag: Box::new(0),
            links: HashSet::new(),
            bufs: Vec::new(),
            memory: Vec::new(),
            temp_bufs: 0,
            budget,
            pending: VecDeque::new(),
            sent: Vec::new(),
            flushes: Vec::new(),
            last: None,
            buffered: false,
            errors: RefCell::new(Vec::new()),
            tasks: VecDeque::new(),
            posted: 0,
            overflow: false,
            blocked: false,
            threads: None,
        }
    }

    fn tag(&self) -> *mut c_void {
        ptr::from_ref::<u8>(&self.tag).cast_mut().cast()
    }

    fn alloc_buf(&mut self) -> *mut ngx_buf_t {
        let b = Box::into_raw(Box::new(unsafe { core::mem::zeroed::<ngx_buf_t>() }));
        self.bufs.push(b);
        b
    }

    /// The write filter: sends what the client takes of the pending buffers.
    fn send(&mut self) -> ngx_int_t {
        let mut budget = self.budget.unwrap_or(usize::MAX);

        while let Some(&b) = self.pending.front() {
            let b = unsafe { &mut *b };
            let flags = unsafe { self.buf_flags(b) };

            if flags.in_memory {
                let size = unsafe { b.last.offset_from(b.pos) } as usize;
                let n = size.min(budget);
                self.sent
                    .extend_from_slice(unsafe { core::slice::from_raw_parts(b.pos, n) });
                b.pos = unsafe { b.pos.add(n) };
                budget -= n;
                if n < size {
                    break;
                }
            }

            if flags.flush {
                self.flushes.push(self.sent.len());
            }
            if flags.last_buf {
                self.last = Some(self.sent.len());
            }
            self.pending.pop_front();
        }

        if self.pending.is_empty() {
            NGX_OK as ngx_int_t
        } else {
            NGX_AGAIN as ngx_int_t
        }
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        // The request pool is destroyed.
        for cl in self.links.drain() {
            drop(unsafe { Box::from_raw(cl) });
        }
        for b in self.bufs.drain(..) {
            drop(unsafe { Box::from_raw(b) });
        }
        for (p, layout) in self.memory.drain(..) {
            unsafe { dealloc(p, layout) };
        }

        // NGINX stops its thread pools when the worker exits.
        if let Some(Threads { tasks, workers, .. }) = self.threads.take() {
            drop(tasks);
            for worker in workers {
                worker.join().unwrap();
            }
        }
    }
}

impl Host for Model {
    unsafe fn chain_add_copy(
        &mut self,
        chain: &mut *mut ngx_chain_t,
        mut input: *mut ngx_chain_t,
    ) -> bool {
        let mut ll: *mut *mut ngx_chain_t = chain;
        unsafe {
            while !(*ll).is_null() {
                ll = &raw mut (**ll).next;
            }
            while !input.is_null() {
                let cl = self.alloc_chain_link();
                (*cl).buf = (*input).buf;
                *ll = cl;
                ll = &raw mut (*cl).next;
                input = (*input).next;
            }
            *ll = ptr::null_mut();
        }
        true
    }

    unsafe fn update_chains(
        &mut self,
        free: &mut *mut ngx_chain_t,
        busy: &mut *mut ngx_chain_t,
        out: &mut *mut ngx_chain_t,
    ) {
        unsafe {
            if !out.is_null() {
                if busy.is_null() {
                    *busy = *out;
                } else {
                    let mut cl = *busy;
                    while !(*cl).next.is_null() {
                        cl = (*cl).next;
                    }
                    (*cl).next = *out;
                }
                *out = ptr::null_mut();
            }

            while !busy.is_null() {
                let cl = *busy;
                let b = (*cl).buf;

                if (*b).tag != self.tag() {
                    *busy = (*cl).next;
                    self.free_chain(cl);
                    continue;
                }

                let size = if self.buf_flags(b).in_memory {
                    (*b).last.offset_from((*b).pos) as off_t
                } else {
                    (*b).file_last - (*b).file_pos
                };
                if size != 0 {
                    break;
                }

                (*b).pos = (*b).start;
                (*b).last = (*b).start;

                *busy = (*cl).next;
                (*cl).next = *free;
                *free = cl;
            }
        }
    }

    fn alloc_chain_link(&mut self) -> *mut ngx_chain_t {
        let cl = Box::into_raw(Box::new(ngx_chain_t {
            buf: ptr::null_mut(),
            next: ptr::null_mut(),
        }));
        self.links.insert(cl);
        cl
    }

    unsafe fn free_chain(&mut self, cl: *mut ngx_chain_t) {
        assert!(self.links.remove(&cl), "freed a chain link it does not own");
        drop(unsafe { Box::from_raw(cl) });
    }

    fn create_temp_buf(&mut self, size: usize) -> *mut ngx_buf_t {
        let layout = Layout::array::<u8>(size).unwrap();
        let p = unsafe { alloc(layout) };
        assert!(!p.is_null());
        self.memory.push((p, layout));
        self.temp_bufs += 1;

        let tag = self.tag();
        let b = unsafe { &mut *self.alloc_buf() };
        b.start = p;
        b.pos = p;
        b.last = p;
        b.end = unsafe { p.add(size) };
        b.set_temporary(1);
        b.set_recycled(1);
        b.tag = tag;
        b
    }

    fn calloc_buf(&mut self) -> *mut ngx_buf_t {
        self.alloc_buf()
    }

    unsafe fn next_body_filter(&mut self, mut chain: *mut ngx_chain_t) -> ngx_int_t {
        while !chain.is_null() {
            unsafe {
                self.pending.push_back((*chain).buf);
                chain = (*chain).next;
            }
        }
        self.send()
    }

    fn set_buffered(&mut self, on: bool) {
        self.buffered = on;
    }

    fn post_task(&mut self, task: OwnedParallelTask) -> Option<OwnedParallelTask> {
        if self.overflow {
            return Some(task);
        }
        match &self.threads {
            Some(threads) => threads.tasks.send(task).unwrap(),
            None => self.tasks.push_back(task),
        }
        self.posted += 1;
        None
    }

    fn set_blocked(&mut self, on: bool) {
        assert_ne!(self.blocked, on, "blocked twice or unblocked twice");
        self.blocked = on;
    }

    unsafe fn buf_flags(&self, b: *const ngx_buf_t) -> BufFlags {
        let b = unsafe { &*b };
        BufFlags {
            in_memory: b.temporary() != 0 || b.memory() != 0 || b.mmap() != 0,
            in_file: b.in_file() != 0,
            last_buf: b.last_buf() != 0,
            flush: b.flush() != 0,
        }
    }

    unsafe fn set_flush(&self, b: *mut ngx_buf_t) {
        unsafe { (*b).set_flush(1) };
    }

    unsafe fn set_last_buf(&self, b: *mut ngx_buf_t) {
        let b = unsafe { &mut *b };
        if b.pos == b.last {
            b.set_temporary(0);
        }
        b.set_last_buf(1);
    }

    fn error(&self, args: fmt::Arguments<'_>) {
        self.errors.borrow_mut().push(args.to_string());
    }

    fn debug(&self, _args: fmt::Arguments<'_>) {}
}

/// One buffer of a response body, as an upstream filter passes it.
#[derive(Copy, Clone)]
enum Part<'a> {
    Data(&'a [u8]),
    Flush(&'a [u8]),
    Last(&'a [u8]),
    /// A buffer that only refers to a file.
    File,
}

/// The filters above the zstd one, which own the body buffers.
#[derive(Default)]
struct Upstream {
    /// Buffers passed on and not yet drained, with the memory they point to.
    held: Vec<(*mut ngx_buf_t, *mut [u8])>,
}

impl Upstream {
    /// Passes `parts` to the filter in one chain, then frees the chain links
    /// and every buffer the filter has drained.
    fn send(&mut self, filter: &mut BodyFilter<Model>, parts: &[Part<'_>]) -> ngx_int_t {
        let mut links = Vec::new();
        for part in parts {
            let b = Box::into_raw(Box::new(unsafe { core::mem::zeroed::<ngx_buf_t>() }));
            let data = Box::into_raw(Box::<[u8]>::from(match part {
                Part::Data(d) | Part::Flush(d) | Part::Last(d) => *d,
                Part::File => &[],
            }));

            let buf = unsafe { &mut *b };
            let start = data.cast::<u8>();
            buf.start = start;
            buf.pos = start;
            buf.last = unsafe { start.add(data.len()) };
            buf.end = buf.last;
            match part {
                Part::Data(_) => buf.set_temporary(1),
                Part::Flush(d) | Part::Last(d) => {
                    buf.set_temporary(!d.is_empty() as u32);
                    if matches!(part, Part::Flush(_)) {
                        buf.set_flush(1);
                    } else {
                        buf.set_last_buf(1);
                    }
                }
                Part::File => {
                    buf.set_in_file(1);
                    buf.file_last = 100;
                }
            }
            self.held.push((b, data));

            links.push(Box::into_raw(Box::new(ngx_chain_t {
                buf: b,
                next: ptr::null_mut(),
            })));
        }
        for pair in links.windows(2) {
            unsafe { (*pair[0]).next = pair[1] };
        }

        let head = links.first().copied().unwrap_or(ptr::null_mut());
        let rc = unsafe { filter.filter(head) };

        for cl in links {
            drop(unsafe { Box::from_raw(cl) });
        }
        self.release_drained();
        rc
    }

    /// A write event: the filter is called again without new input.
    fn resume(&mut self, filter: &mut BodyFilter<Model>) -> ngx_int_t {
        let rc = if filter.is_done() {
            // The zstd filter passes calls through once it is done.
            unsafe { filter.host_mut().next_body_filter(ptr::null_mut()) }
        } else {
            unsafe { filter.filter(ptr::null_mut()) }
        };
        self.release_drained();
        rc
    }

    /// The thread pool runs the oldest posted task, and the event loop reports
    /// its completion and calls the filter again.
    fn complete(&mut self, filter: &mut BodyFilter<Model>) -> ngx_int_t {
        let task = filter
            .host_mut()
            .tasks
            .pop_front()
            .expect("a task is posted");
        task.run();
        filter.task_done();
        self.resume(filter)
    }

    /// Completes posted tasks and sends write events until the response ends.
    fn finish(&mut self, filter: &mut BodyFilter<Model>) {
        let mut calls = 0;
        while filter.host().last.is_none() || !filter.host().tasks.is_empty() {
            calls += 1;
            assert!(calls < 100_000, "the response never completes");
            let rc = if filter.host().tasks.is_empty() {
                self.resume(filter)
            } else {
                self.complete(filter)
            };
            assert_ne!(rc, NGX_ERROR as ngx_int_t);
        }
    }

    fn release_drained(&mut self) {
        self.held.retain(|&(b, data)| {
            let drained = unsafe { (*b).pos == (*b).last && (*b).file_pos == (*b).file_last };
            if drained {
                drop(unsafe { Box::from_raw(b) });
                drop(unsafe { Box::from_raw(data) });
            }
            !drained
        });
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        for (b, data) in self.held.drain(..) {
            drop(unsafe { Box::from_raw(b) });
            drop(unsafe { Box::from_raw(data) });
        }
    }
}

fn settings(bufs_num: usize, bufs_size: usize) -> Settings {
    Settings {
        encoder: EncoderConfig::default().with_level(Level::try_from(5).unwrap()),
        input_size: InputSize::Unknown,
        reuse_memory: true,
        retention: RetentionPolicy::Bounded { max_bytes: 1 << 20 },
        bufs_num,
        bufs_size,
        parallel: None,
    }
}

fn filter(budget: Option<usize>, bufs_num: usize, bufs_size: usize) -> BodyFilter<Model> {
    BodyFilter::new(Model::new(budget), settings(bufs_num, bufs_size))
}

/// The smallest segment there is.
const SEGMENT: usize = 64 << 10;

/// A filter with `zstd_thread_pool`, cutting the response into 64 KiB
/// segments and keeping at most `tasks` of them at once.
fn parallel_filter(budget: Option<usize>, tasks: usize) -> BodyFilter<Model> {
    let mut settings = settings(4, 4096);
    settings.encoder = settings.encoder.with_level(Level::try_from(1).unwrap());
    settings.parallel = Some(Parallel {
        segment_size: SegmentSize::try_from(SEGMENT).unwrap(),
        tasks: TaskCount::try_from(tasks).unwrap(),
    });
    BodyFilter::new(Model::new(budget), settings)
}

fn text(len: usize) -> Vec<u8> {
    let words = ["zstd ", "nginx ", "filter ", "chain ", "buffer ", "stream "];
    let mut seed = 7u32;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        out.extend_from_slice(words[(seed >> 16) as usize % words.len()].as_bytes());
    }
    out.truncate(len);
    out
}

fn decode(stream: &[u8]) -> Vec<u8> {
    Decompressor::default().decompress(stream).unwrap()
}

/// Decodes the start of an unfinished stream, up to its last flush.
fn decode_prefix(stream: &[u8]) -> Vec<u8> {
    let mut decoder = Decompressor::default();
    let mut session = decoder.start();
    let mut input = stream;
    let mut decoded = Vec::new();
    let mut out = vec![0; 1 << 16];
    loop {
        let progress = session
            .process(input, &mut out, DecodeOperation::Process)
            .unwrap();
        input = &input[progress.consumed..];
        decoded.extend_from_slice(&out[..progress.produced]);
        if progress.status == DecoderStatus::NeedsInput {
            return decoded;
        }
    }
}

const OK: ngx_int_t = NGX_OK as ngx_int_t;

#[test]
fn one_call_round_trip() {
    let (a, b, c) = (text(1500), text(700), text(300));
    let mut f = filter(None, 4, 4096);
    let mut up = Upstream::default();

    assert_eq!(
        up.send(&mut f, &[Part::Data(&a), Part::Data(&b), Part::Last(&c)]),
        OK
    );

    let model = f.host();
    assert!(f.is_done());
    assert_eq!(decode(&model.sent), [a, b, c].concat());
    assert_eq!(model.last, Some(model.sent.len()));
    assert!(!model.buffered);
    assert_eq!(f.sizes(), Some((2500, model.sent.len() as u64)));
    assert!(model.errors.borrow().is_empty());
}

#[test]
fn drained_input_is_never_touched_again() {
    // Upstream frees every buffer once the filter has drained it, as the SSI
    // filter recycles its own: reading one later is a use-after-free.
    let chunks: Vec<Vec<u8>> = (0..4).map(|i| text(400 + 100 * i)).collect();
    let mut f = filter(None, 4, 4096);
    let mut up = Upstream::default();

    for chunk in &chunks {
        assert_eq!(up.send(&mut f, &[Part::Data(chunk)]), OK);
        assert!(f.host().buffered);
    }
    assert_eq!(up.send(&mut f, &[Part::Last(&[])]), OK);

    assert_eq!(decode(&f.host().sent), chunks.concat());
    assert!(up.held.is_empty());
}

#[test]
fn slow_client_with_few_small_buffers() {
    let body = text(3000);
    let mut f = filter(Some(5), 2, 64);
    let mut up = Upstream::default();

    up.send(&mut f, &[Part::Data(&body)]);
    up.send(&mut f, &[Part::Last(&[])]);

    let mut calls = 0;
    while f.host().last.is_none() {
        calls += 1;
        assert!(calls < 100_000, "the response never completes");
        assert_ne!(up.resume(&mut f), NGX_ERROR as ngx_int_t);
    }

    let model = f.host();
    assert_eq!(model.temp_bufs, 2);
    assert_eq!(decode(&model.sent), body);
    assert!(up.held.is_empty());
}

#[test]
fn flush_makes_everything_so_far_decodable() {
    let (a, b) = (text(1200), text(800));
    let mut f = filter(None, 4, 4096);
    let mut up = Upstream::default();

    assert_eq!(up.send(&mut f, &[Part::Flush(&a)]), OK);
    let model = f.host();
    assert_eq!(model.flushes, [model.sent.len()]);
    assert_eq!(decode_prefix(&model.sent), a);
    assert!(!model.buffered);

    // A flush with nothing new still reaches the client.
    assert_eq!(up.send(&mut f, &[Part::Flush(&[])]), OK);
    assert_eq!(f.host().flushes.len(), 2);

    assert_eq!(up.send(&mut f, &[Part::Last(&b)]), OK);
    assert_eq!(decode(&f.host().sent), [a, b].concat());
}

#[test]
fn empty_and_zero_length_buffers() {
    let a = text(500);
    let mut f = filter(None, 4, 4096);
    let mut up = Upstream::default();

    let parts = [
        Part::Data(&[]),
        Part::Data(&a),
        Part::Data(&[]),
        Part::Last(&[]),
    ];
    assert_eq!(up.send(&mut f, &parts), OK);
    assert_eq!(decode(&f.host().sent), a);

    let mut f = filter(None, 4, 4096);
    assert_eq!(up.send(&mut f, &[Part::Last(&[])]), OK);
    assert!(decode(&f.host().sent).is_empty());
}

#[test]
fn file_buffers_are_rejected() {
    let mut f = filter(None, 4, 4096);
    let mut up = Upstream::default();

    assert_eq!(up.send(&mut f, &[Part::File]), NGX_ERROR as ngx_int_t);
    assert!(f.is_done());
    assert_eq!(f.sizes(), None);
    assert_eq!(*f.host().errors.borrow(), ["zstd filter got a file buffer"]);
}

#[test]
fn reused_encoders_produce_identical_responses() {
    let body = text(2000);
    let mut responses = Vec::new();
    for _ in 0..2 {
        let mut f = filter(None, 4, 4096);
        let mut up = Upstream::default();
        assert_eq!(up.send(&mut f, &[Part::Last(&body)]), OK);
        responses.push(f.host().sent.clone());
    }
    assert_eq!(responses[0], responses[1]);
    assert_eq!(decode(&responses[0]), body);
}

#[test]
fn known_length_is_declared() {
    let body = text(2000);
    let mut settings = settings(4, 4096);
    settings.input_size = InputSize::Exact(body.len() as u64);
    let mut f = BodyFilter::new(Model::new(None), settings);
    let mut up = Upstream::default();

    assert_eq!(up.send(&mut f, &[Part::Data(&body[..500])]), OK);
    assert_eq!(up.send(&mut f, &[Part::Last(&body[500..])]), OK);
    assert_eq!(decode(&f.host().sent), body);
    assert!(f.host().errors.borrow().is_empty());
}

/// A response whose body is not as long as its `Content-Length` said.
fn mismatched_length(mut settings: Settings, declared: usize, parts: &[Part<'_>]) {
    settings.input_size = InputSize::Exact(declared as u64);
    let mut f = BodyFilter::new(Model::new(None), settings);
    let mut up = Upstream::default();

    let mut rc = OK;
    for part in parts {
        rc = up.send(&mut f, &[*part]);
        if rc == NGX_ERROR as ngx_int_t {
            break;
        }
        while !f.host().tasks.is_empty() {
            rc = up.complete(&mut f);
        }
    }
    assert_eq!(rc, NGX_ERROR as ngx_int_t);
    assert!(f.is_done());
    assert_eq!(f.sizes(), None);
    assert_eq!(f.host().last, None, "the response was ended");
    assert_eq!(f.host().errors.borrow().len(), 1);
}

#[test]
fn a_length_that_does_not_match_fails() {
    let body = text(2000);
    let len = body.len();
    let serial = || settings(4, 4096);

    // All of it in the first call, which ends the frame.
    mismatched_length(serial(), len + 1, &[Part::Last(&body)]);
    mismatched_length(serial(), len - 1, &[Part::Last(&body)]);
    // In parts.
    let parts = [Part::Data(&body[..500]), Part::Last(&body[500..])];
    mismatched_length(serial(), len + 1, &parts);
    mismatched_length(serial(), len - 1, &parts);

    let mut parallel = settings(4, 4096);
    parallel.parallel = Some(Parallel {
        segment_size: SegmentSize::try_from(SEGMENT).unwrap(),
        tasks: TaskCount::try_from(2).unwrap(),
    });
    let body = text(SEGMENT + 1000);
    let len = body.len();
    mismatched_length(parallel, len + 1, &[Part::Last(&body)]);
    mismatched_length(parallel, len - 1, &[Part::Last(&body)]);
    let parts = [Part::Data(&body[..SEGMENT]), Part::Last(&body[SEGMENT..])];
    mismatched_length(parallel, len + 1, &parts);
    mismatched_length(parallel, len - 1, &parts);
}

#[test]
fn parallel_segments_round_trip() {
    // Three full segments and a short one, arriving in 16 KiB chunks.
    let body = text(3 * SEGMENT + 1000);
    let mut f = parallel_filter(None, 2);
    let mut up = Upstream::default();

    let mut held = false;
    for chunk in body.chunks(16 << 10) {
        assert_ne!(
            up.send(&mut f, &[Part::Data(chunk)]),
            NGX_ERROR as ngx_int_t
        );
        assert!(f.host().buffered);
        // With both tasks posted, the next segment waits for one of them: the
        // filter keeps the input it cannot take yet, until a task completes.
        while !up.held.is_empty() {
            held = true;
            assert_eq!(f.host().tasks.len(), 2);
            up.complete(&mut f);
        }
    }
    assert!(held, "the window never filled up");
    up.send(&mut f, &[Part::Last(&[])]);
    up.finish(&mut f);

    let model = f.host();
    assert_eq!(decode(&model.sent), body);
    assert_eq!(model.posted, 4);
    assert_eq!(model.last, Some(model.sent.len()));
    assert!(!model.buffered);
    assert!(!model.blocked);
    assert_eq!(
        f.sizes(),
        Some((body.len() as u64, model.sent.len() as u64))
    );
    assert!(model.errors.borrow().is_empty());
    assert!(up.held.is_empty());
}

#[test]
fn parallel_short_response_is_one_task() {
    let body = text(3000);
    let mut f = parallel_filter(None, 4);
    let mut up = Upstream::default();

    // The response waits for its task, holding nothing upstream owns.
    assert_eq!(up.send(&mut f, &[Part::Last(&body)]), OK);
    assert!(f.host().sent.is_empty());
    assert!(f.host().buffered && f.host().blocked);
    assert!(up.held.is_empty());

    assert_eq!(up.complete(&mut f), OK);
    let model = f.host();
    assert!(f.is_done());
    assert_eq!(model.posted, 1);
    assert_eq!(decode(&model.sent), body);
    assert!(!model.buffered && !model.blocked);
}

#[test]
fn parallel_flush_waits_for_its_segments() {
    let (a, b) = (text(SEGMENT + 500), text(800));
    let mut f = parallel_filter(None, 4);
    let mut up = Upstream::default();

    assert_eq!(up.send(&mut f, &[Part::Flush(&a)]), OK);
    assert!(f.host().flushes.is_empty());
    while !f.host().tasks.is_empty() {
        up.complete(&mut f);
    }
    let model = f.host();
    assert_eq!(model.flushes, [model.sent.len()]);
    assert_eq!(decode_prefix(&model.sent), a);
    assert!(!model.buffered && !model.blocked);

    up.send(&mut f, &[Part::Last(&b)]);
    up.finish(&mut f);
    assert_eq!(decode(&f.host().sent), [a, b].concat());
}

#[test]
fn parallel_request_stays_buffered_until_every_task_is_reported() {
    let body = text(2 * SEGMENT);
    let mut f = parallel_filter(None, 2);
    let mut up = Upstream::default();

    up.send(&mut f, &[Part::Last(&body)]);
    let tasks: Vec<_> = f.host_mut().tasks.drain(..).collect();
    assert_eq!(tasks.len(), 2);
    // Both tasks run, but the event loop has reported only one of them.
    tasks.into_iter().for_each(|task| task.run());
    f.task_done();
    up.resume(&mut f);

    // The stream is complete, but the event loop still refers to the request.
    assert!(f.is_done());
    assert_eq!(f.host().last, Some(f.host().sent.len()));
    assert!(f.host().buffered && f.host().blocked);

    f.task_done();
    assert!(!f.host().buffered && !f.host().blocked);
    assert_eq!(decode(&f.host().sent), body);
}

#[test]
fn parallel_tasks_run_inline_when_the_pool_is_full() {
    let body = text(2 * SEGMENT + 100);
    let mut f = parallel_filter(None, 2);
    f.host_mut().overflow = true;
    let mut up = Upstream::default();

    assert_eq!(up.send(&mut f, &[Part::Last(&body)]), OK);
    let model = f.host();
    assert!(f.is_done());
    assert_eq!(model.posted, 0);
    assert!(!model.blocked && !model.buffered);
    assert_eq!(decode(&model.sent), body);
}

#[test]
fn parallel_slow_client_with_few_small_buffers() {
    let body = text(SEGMENT + 3000);
    let mut settings = settings(2, 64);
    settings.parallel = Some(Parallel {
        segment_size: SegmentSize::try_from(SEGMENT).unwrap(),
        tasks: TaskCount::ONE,
    });
    let mut f = BodyFilter::new(Model::new(Some(997)), settings);
    let mut up = Upstream::default();

    up.send(&mut f, &[Part::Data(&body)]);
    up.send(&mut f, &[Part::Last(&[])]);
    up.finish(&mut f);

    let model = f.host();
    assert_eq!(model.temp_bufs, 2);
    assert_eq!(decode(&model.sent), body);
    assert!(!model.buffered && !model.blocked);
    assert!(up.held.is_empty());
}

#[test]
fn abandoned_response_lets_its_tasks_finish() {
    let body = text(2 * SEGMENT);
    let mut f = parallel_filter(None, 2);
    let mut up = Upstream::default();

    up.send(&mut f, &[Part::Last(&body)]);
    assert_eq!(f.host().tasks.len(), 2);

    // The request is terminated: its tasks still run, cancelled.
    f.abandon();
    assert!(f.is_done());
    while let Some(task) = f.host_mut().tasks.pop_front() {
        task.run();
        f.task_done();
    }
    assert!(!f.host().blocked && !f.host().buffered);
    assert_eq!(f.sizes(), None);
}

/// The event loop reports a task completed on a thread, waiting for one if
/// `wait`, and runs the filter again. Returns whether there was one.
fn report(f: &mut BodyFilter<Model>, up: &mut Upstream, wait: bool) -> bool {
    let threads = f.host().threads.as_ref().unwrap();
    let done = if wait {
        threads.done.recv().is_ok()
    } else {
        threads.done.try_recv().is_ok()
    };
    if done {
        f.task_done();
        assert_ne!(up.resume(f), NGX_ERROR as ngx_int_t);
    }
    done
}

#[test]
fn parallel_tasks_on_threads() {
    // Segments are encoded on two threads while the filter takes input,
    // collects finished segments and sends them on this one.
    let body = text(3 * SEGMENT + 1000);
    let mut f = parallel_filter(None, 2);
    f.host_mut().threads = Some(Threads::new(2));
    let mut up = Upstream::default();
    let mut reported = 0;

    for chunk in body.chunks(16 << 10) {
        assert_ne!(
            up.send(&mut f, &[Part::Data(chunk)]),
            NGX_ERROR as ngx_int_t
        );
        while report(&mut f, &mut up, false) {
            reported += 1;
        }
        // With the window full, the input waits for a task to complete.
        while !up.held.is_empty() {
            assert!(report(&mut f, &mut up, true));
            reported += 1;
        }
    }
    up.send(&mut f, &[Part::Last(&[])]);
    while f.host().last.is_none() || reported < f.host().posted {
        assert!(
            report(&mut f, &mut up, true),
            "the response never completes"
        );
        reported += 1;
    }

    let model = f.host();
    assert_eq!(decode(&model.sent), body);
    assert_eq!(model.posted, 4);
    assert!(!model.buffered && !model.blocked);
    assert!(model.errors.borrow().is_empty());
}
