//! The body filter's state machine.
//!
//! Everything it needs from NGINX (chain links, buffers, the next body filter,
//! the thread pool) goes through [`Host`]. In NGINX that is a thin wrapper over
//! the C API; in tests it is a Rust model of the same API, which lets the
//! pointer handling here run under Miri.
//!
//! With `zstd_thread_pool`, the response is encoded by a parallel session:
//! it cuts the input into segments and queues each one as a task, which the
//! filter posts to the thread pool. Completed segments are collected by the
//! next call of the filter, so the host calls it again whenever a task
//! completes ([`BodyFilter::task_done`]).
//!
//! With `zstd_dict_file`, the response is encoded by a serial session with the
//! dictionary, also when `zstd_thread_pool` is set: mzstd's parallel
//! compressor does not take dictionaries.

use core::fmt;
use core::mem::MaybeUninit;
use core::{ptr, slice};
use std::sync::Arc;

use mzstd::compressor::parallel::{
    OwnedParallelTask, ParallelConfig, ParallelEncoderSessionOwned, ParallelEncoderStatus,
    ParallelProgress, ParallelTaskQueue, SegmentSize, TaskCount,
};
use mzstd::dictionary::PreparedDictionary;
use mzstd::{
    Compressor, EncoderConfig, EncoderSessionOwned, EncoderStatus, InputSize, Operation, Progress,
    StreamConfig,
};
use ngx::ffi::{NGX_AGAIN, NGX_ERROR, NGX_OK, ngx_buf_t, ngx_chain_t, ngx_int_t};

use crate::encoder_pool::{self, RetentionPolicy};

/// The flags of an `ngx_buf_t` the filter looks at.
#[derive(Copy, Clone, Debug, Default)]
pub struct BufFlags {
    pub in_memory: bool,
    pub in_file: bool,
    pub last_buf: bool,
    pub flush: bool,
}

/// The NGINX API the body filter uses, for one request.
///
/// Buffers and chain links come from the request pool and live until the
/// request ends, unless they are explicitly freed.
pub trait Host {
    /// `ngx_chain_add_copy()`: appends links for the buffers of `input`.
    ///
    /// # Safety
    ///
    /// `chain` and `input` must be valid chains.
    unsafe fn chain_add_copy(
        &mut self,
        chain: &mut *mut ngx_chain_t,
        input: *mut ngx_chain_t,
    ) -> bool;

    /// `ngx_chain_update_chains()` with this module's buffer tag.
    ///
    /// # Safety
    ///
    /// The arguments must be valid chains.
    unsafe fn update_chains(
        &mut self,
        free: &mut *mut ngx_chain_t,
        busy: &mut *mut ngx_chain_t,
        out: &mut *mut ngx_chain_t,
    );

    /// `ngx_alloc_chain_link()`.
    fn alloc_chain_link(&mut self) -> *mut ngx_chain_t;

    /// `ngx_free_chain()`: `cl` must not be used afterwards.
    ///
    /// # Safety
    ///
    /// `cl` must be a link allocated for this request.
    unsafe fn free_chain(&mut self, cl: *mut ngx_chain_t);

    /// `ngx_create_temp_buf()`, tagged as this module's and marked recycled.
    /// The memory it points to is uninitialized.
    fn create_temp_buf(&mut self, size: usize) -> *mut ngx_buf_t;

    /// `ngx_calloc_buf()`: an empty buffer, for flags only.
    fn calloc_buf(&mut self) -> *mut ngx_buf_t;

    /// Passes `chain` to the next body filter.
    ///
    /// # Safety
    ///
    /// `chain` must be a valid chain, or null.
    unsafe fn next_body_filter(&mut self, chain: *mut ngx_chain_t) -> ngx_int_t;

    /// Sets or clears this module's bit in `r->connection->buffered`.
    fn set_buffered(&mut self, on: bool);

    /// Queues `task` on the thread pool. Returns it if it cannot be queued.
    ///
    /// Once the task has run, the host calls [`BodyFilter::task_done`] and
    /// then the filter again, from the thread that runs the filter.
    fn post_task(&mut self, task: OwnedParallelTask) -> Option<OwnedParallelTask>;

    /// Keeps the request from being freed while tasks are posted
    /// (`r->main->blocked`), or lets it go again.
    fn set_blocked(&mut self, on: bool);

    /// # Safety
    ///
    /// `b` must be a valid buffer.
    unsafe fn buf_flags(&self, b: *const ngx_buf_t) -> BufFlags;

    /// # Safety
    ///
    /// `b` must be a valid buffer.
    unsafe fn set_flush(&self, b: *mut ngx_buf_t);

    /// Marks `b` as the last buffer; an empty one stops claiming to hold data.
    ///
    /// # Safety
    ///
    /// `b` must be a valid buffer.
    unsafe fn set_last_buf(&self, b: *mut ngx_buf_t);

    /// Logs at the `alert` level.
    fn error(&self, args: fmt::Arguments<'_>);

    /// Logs at the `debug` level, for `NGX_LOG_DEBUG_HTTP`.
    fn debug(&self, args: fmt::Arguments<'_>);
}

/// What the filter needs from the location configuration.
#[derive(Clone, Debug)]
pub struct Settings {
    pub encoder: EncoderConfig,
    pub input_size: InputSize,
    pub reuse_memory: bool,
    pub retention: RetentionPolicy,
    /// `zstd_buffers`.
    pub bufs_num: usize,
    pub bufs_size: usize,
    /// Set when `zstd_thread_pool` is. Not used with a dictionary.
    pub parallel: Option<Parallel>,
    /// `zstd_dict_file`, prepared for `encoder`.
    pub dictionary: Option<Arc<PreparedDictionary>>,
}

/// How a response is split up for the thread pool.
#[derive(Copy, Clone, Debug)]
pub struct Parallel {
    /// `zstd_parallel_segment_size`.
    pub segment_size: SegmentSize,
    /// `zstd_parallel_tasks`: segments queued, running or waiting to be
    /// sent at once.
    pub tasks: TaskCount,
}

/// An error that has already been logged; the response must be aborted.
struct Failed;

/// What the filter loop should do after a step.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Step {
    /// The step completed; carry on with the next one.
    Ok,
    /// Repeat the inner loop.
    Again,
    /// Nothing more can be done in the inner loop.
    Declined,
}

/// The encoder of one response.
enum Session {
    /// Boxed: its type has room for a dictionary of its own, which takes
    /// kilobytes even when there is none.
    Serial(Box<EncoderSessionOwned>),
    /// A serial session with `zstd_dict_file`.
    Dictionary(EncoderSessionOwned<Arc<PreparedDictionary>>),
    Parallel {
        /// Boxed: it is far larger than a serial session.
        session: Box<ParallelEncoderSessionOwned>,
        /// Segments the session has queued and the filter has not posted yet.
        queue: ParallelTaskQueue,
        /// For a compressor from the cache, which takes it back.
        lease: Option<encoder_pool::ParallelLease>,
    },
}

/// What the encoder needs next; `Pending` only comes from a parallel session.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Status {
    NeedsInput,
    NeedsOutput,
    /// Waiting for posted tasks to complete.
    Pending,
    Finished,
}

/// What one call of the encoder did.
struct Outcome {
    consumed: usize,
    produced: usize,
    status: Status,
}

impl From<Progress> for Outcome {
    fn from(progress: Progress) -> Self {
        Self {
            consumed: progress.consumed,
            produced: progress.produced,
            status: match progress.status {
                EncoderStatus::NeedsInput => Status::NeedsInput,
                EncoderStatus::NeedsOutput => Status::NeedsOutput,
                EncoderStatus::Finished => Status::Finished,
            },
        }
    }
}

impl From<ParallelProgress> for Outcome {
    fn from(progress: ParallelProgress) -> Self {
        Self {
            consumed: progress.consumed,
            produced: progress.produced,
            status: match progress.status {
                ParallelEncoderStatus::NeedsInput => Status::NeedsInput,
                ParallelEncoderStatus::NeedsOutput => Status::NeedsOutput,
                ParallelEncoderStatus::Pending => Status::Pending,
                ParallelEncoderStatus::Finished => Status::Finished,
            },
        }
    }
}

/// The state of one response's body filter.
pub struct BodyFilter<H: Host> {
    host: H,
    settings: Settings,
    session: Option<Session>,

    /// Input not yet handed to the encoder.
    input: *mut ngx_chain_t,
    /// The input buffer being consumed; null once it has been drained.
    ///
    /// A drained buffer belongs to the filter that produced it again and may be
    /// refilled with later data, so it must never be read after that point.
    in_buf: *mut ngx_buf_t,

    free: *mut ngx_chain_t,
    busy: *mut ngx_chain_t,
    out: *mut ngx_chain_t,
    out_tail: *mut ngx_chain_t,
    /// The output buffer being filled; null once it has been queued.
    out_buf: *mut ngx_buf_t,
    bufs: usize,

    operation: Operation,
    /// The encoder holds output that did not fit the last buffer.
    redo: bool,
    /// A posted task has completed: the encoder may have output to deliver.
    collect: bool,
    nomem: bool,
    done: bool,
    success: bool,

    /// The encoder holds data that has not been sent.
    buffered: bool,
    /// Tasks posted to the thread pool whose completion the host has not
    /// reported yet.
    in_flight: usize,

    bytes_in: u64,
    bytes_out: u64,
}

impl<H: Host> Drop for BodyFilter<H> {
    fn drop(&mut self) {
        self.release_encoder();
    }
}

impl<H: Host> BodyFilter<H> {
    pub fn new(host: H, settings: Settings) -> Self {
        Self {
            host,
            settings,
            session: None,
            input: ptr::null_mut(),
            in_buf: ptr::null_mut(),
            free: ptr::null_mut(),
            busy: ptr::null_mut(),
            out: ptr::null_mut(),
            out_tail: ptr::null_mut(),
            out_buf: ptr::null_mut(),
            bufs: 0,
            operation: Operation::Continue,
            redo: false,
            collect: false,
            nomem: false,
            done: false,
            success: false,
            buffered: false,
            in_flight: 0,
            bytes_in: 0,
            bytes_out: 0,
        }
    }

    /// Whether the response has been encoded completely, or has failed.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Uncompressed and compressed sizes of a successfully encoded response.
    pub fn sizes(&self) -> Option<(u64, u64)> {
        self.success.then_some((self.bytes_in, self.bytes_out))
    }

    #[cfg(test)]
    pub fn host(&self) -> &H {
        &self.host
    }

    #[cfg(test)]
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// The body filter: encodes `input` and passes the result on.
    ///
    /// # Safety
    ///
    /// `input` must be a valid chain, or null, as the filter chain passes it.
    pub unsafe fn filter(&mut self, input: *mut ngx_chain_t) -> ngx_int_t {
        match unsafe { self.run(input) } {
            Ok(rc) => rc,
            Err(Failed) => {
                self.done = true;
                self.release_encoder();
                NGX_ERROR as ngx_int_t
            }
        }
    }

    /// Records that a task given to [`Host::post_task`] has run. The filter
    /// must be called next, unless the request is going away.
    ///
    /// The request stays blocked, and buffered, until the last posted task
    /// is reported: the host still refers to the request until then.
    #[cfg_attr(
        not(ngx_feature = "threads"),
        allow(dead_code, reason = "only NGINX with threads has a thread pool")
    )]
    pub fn task_done(&mut self) {
        debug_assert!(self.in_flight > 0);
        self.in_flight -= 1;
        self.collect = true;
        if self.in_flight == 0 {
            self.host.set_blocked(false);
            self.set_buffered(self.buffered);
        }
    }

    /// Stops encoding a response that will not be sent: tasks that have not
    /// started yet are cancelled.
    #[cfg_attr(
        not(ngx_feature = "threads"),
        allow(dead_code, reason = "only NGINX with threads has a thread pool")
    )]
    pub fn abandon(&mut self) {
        self.done = true;
        self.release_encoder();
        self.set_buffered(false);
    }

    fn set_buffered(&mut self, on: bool) {
        self.buffered = on;
        self.host.set_buffered(on || self.in_flight > 0);
    }

    fn release_encoder(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let Settings {
            reuse_memory,
            retention,
            ..
        } = self.settings;
        if !reuse_memory {
            return;
        }
        match session {
            Session::Serial(session) => {
                encoder_pool::release(session.into_compressor(), retention);
            }
            Session::Dictionary(session) => {
                encoder_pool::release(session.into_compressor(), retention);
            }
            Session::Parallel {
                session,
                lease: Some(lease),
                ..
            } => {
                encoder_pool::release_parallel(session.into_compressor(), retention, lease);
            }
            Session::Parallel { lease: None, .. } => {}
        }
    }

    unsafe fn run(&mut self, input: *mut ngx_chain_t) -> Result<ngx_int_t, Failed> {
        if self.session.is_none() {
            self.start()?;
        }

        if !input.is_null() {
            if !unsafe { self.host.chain_add_copy(&mut self.input, input) } {
                return Err(Failed);
            }
            self.set_buffered(true);
        }

        let mut flush = if self.nomem {
            // Push busy buffers downstream to get some of them back.
            if unsafe { self.host.next_body_filter(ptr::null_mut()) } == NGX_ERROR as ngx_int_t {
                return Err(Failed);
            }
            let mut cl = ptr::null_mut();
            unsafe {
                self.host
                    .update_chains(&mut self.free, &mut self.busy, &mut cl)
            };
            self.nomem = false;
            false
        } else {
            !self.busy.is_null()
        };

        loop {
            // Cycle while there is input for the encoder and room for output.
            loop {
                match self.add_data()? {
                    Step::Declined => break,
                    Step::Again => continue,
                    Step::Ok => {}
                }

                if !self.get_buf()? {
                    break;
                }

                if self.compress()? == Step::Ok {
                    break;
                }
            }

            if self.out.is_null() && !flush {
                return Ok(if self.busy.is_null() {
                    NGX_OK as ngx_int_t
                } else {
                    NGX_AGAIN as ngx_int_t
                });
            }

            let rc = unsafe { self.host.next_body_filter(self.out) };
            if rc == NGX_ERROR as ngx_int_t {
                return Err(Failed);
            }

            unsafe {
                self.host
                    .update_chains(&mut self.free, &mut self.busy, &mut self.out)
            };
            self.out_tail = ptr::null_mut();
            self.nomem = false;
            flush = false;

            if self.done {
                return Ok(rc);
            }
        }
    }

    fn start(&mut self) -> Result<(), Failed> {
        let Settings {
            encoder,
            input_size,
            reuse_memory,
            retention,
            parallel,
            ..
        } = self.settings;
        let stream = StreamConfig::from(input_size);

        let session = match (self.settings.dictionary.clone(), parallel) {
            (Some(dictionary), _) => {
                let session = self
                    .serial_compressor()?
                    .into_session_with_dictionary(dictionary, stream)
                    .map_err(|err| self.config_error(&err))?;
                Session::Dictionary(session)
            }

            (None, None) => {
                Session::Serial(Box::new(self.serial_compressor()?.into_session(stream)))
            }

            (
                None,
                Some(Parallel {
                    segment_size,
                    tasks,
                }),
            ) => {
                let config = ParallelConfig::default().with_segment_size(segment_size);
                let compressor = if reuse_memory {
                    encoder_pool::acquire_parallel(encoder, config)
                        .map(|(compressor, lease)| (compressor, Some(lease)))
                } else {
                    encoder_pool::build_parallel(encoder, config)
                        .map(|compressor| (compressor, None))
                };
                let (compressor, lease) = compressor.map_err(|err| self.config_error(&err))?;
                let (session, queue) = compressor.into_session(stream, tasks);
                Session::Parallel {
                    session: Box::new(session),
                    queue,
                    lease,
                }
            }
        };
        // A dictionary takes precedence over the thread pool.
        let parallel = parallel.filter(|_| matches!(session, Session::Parallel { .. }));
        self.session = Some(session);

        self.host.debug(format_args!(
            "zstd encoder initialized: lvl:{} wlog:{:?} size:{:?} reuse:{} retention:{:?} \
             parallel:{:?} dict:{:?}",
            encoder.level(),
            encoder.params().window_log(),
            input_size,
            reuse_memory,
            retention,
            parallel,
            self.settings
                .dictionary
                .as_ref()
                .map(|dictionary| dictionary.id().map_or(0, u32::from))
        ));

        Ok(())
    }

    /// A compressor for a serial session, from the cache if `zstd_reuse_memory`
    /// is on.
    fn serial_compressor(&self) -> Result<Compressor, Failed> {
        let Settings {
            encoder,
            reuse_memory,
            retention,
            ..
        } = self.settings;
        let compressor = if reuse_memory {
            encoder_pool::acquire(encoder, retention)
        } else {
            encoder_pool::build(encoder, retention)
        };
        compressor.map_err(|err| self.config_error(&err))
    }

    fn config_error(&self, err: &dyn fmt::Display) -> Failed {
        self.host
            .error(format_args!("zstd encoder config failed: {err}"));
        Failed
    }

    fn encode_error(&self, err: &dyn fmt::Display) -> Failed {
        self.host.error(format_args!("zstd encoder failed: {err}"));
        Failed
    }

    /// Bytes of the current input buffer still waiting for the encoder.
    fn pending_input<'a>(&self) -> &'a [u8] {
        match unsafe { self.in_buf.as_ref() } {
            Some(b) if b.last > b.pos && unsafe { self.host.buf_flags(b) }.in_memory => unsafe {
                slice::from_raw_parts(b.pos, b.last.offset_from(b.pos) as usize)
            },
            _ => &[],
        }
    }

    /// Takes the next input buffer, if the encoder needs one.
    fn add_data(&mut self) -> Result<Step, Failed> {
        if !self.pending_input().is_empty()
            || self.operation != Operation::Continue
            || self.redo
            || self.collect
        {
            return Ok(Step::Ok);
        }

        let cl = self.input;
        if cl.is_null() {
            return Ok(Step::Declined);
        }

        unsafe {
            self.in_buf = (*cl).buf;
            self.input = (*cl).next;
            self.host.free_chain(cl);
        }

        let b = unsafe { &*self.in_buf };
        let flags = unsafe { self.host.buf_flags(b) };

        if !flags.in_memory && flags.in_file && b.file_last > b.file_pos {
            self.host
                .error(format_args!("zstd filter got a file buffer"));
            return Err(Failed);
        }

        let empty = self.pending_input().is_empty();
        if empty {
            self.in_buf = ptr::null_mut();
        }

        if flags.last_buf {
            self.operation = Operation::End;
        } else if flags.flush {
            self.operation = Operation::Flush;
        } else if empty {
            return Ok(Step::Again);
        }

        Ok(Step::Ok)
    }

    /// Makes sure there is an output buffer with free space. Returns `false`
    /// when all `zstd_buffers` are busy downstream.
    fn get_buf(&mut self) -> Result<bool, Failed> {
        if let Some(b) = unsafe { self.out_buf.as_ref() }
            && b.last < b.end
        {
            return Ok(true);
        }

        if !self.free.is_null() {
            let cl = self.free;
            unsafe {
                self.out_buf = (*cl).buf;
                self.free = (*cl).next;
                self.host.free_chain(cl);
            }
            return Ok(true);
        }

        if self.bufs >= self.settings.bufs_num {
            self.nomem = true;
            return Ok(false);
        }

        let size = self.settings.bufs_size;
        let b = self.host.create_temp_buf(size);
        if b.is_null() {
            return Err(Failed);
        }
        self.out_buf = b;
        self.bufs += 1;

        Ok(true)
    }

    /// Runs the encoder once over the current input and output buffers.
    fn compress(&mut self) -> Result<Step, Failed> {
        let input = self.pending_input();
        let input_len = input.len();
        let out_buf = self.out_buf;
        let (last, room) =
            unsafe { ((*out_buf).last, (*out_buf).end.offset_from((*out_buf).last)) };
        let room = room as usize;
        self.collect = false;

        // Buffer memory is uninitialized until the encoder writes it.
        let output = unsafe { slice::from_raw_parts_mut(last.cast::<MaybeUninit<u8>>(), room) };
        let outcome = match self.session.as_mut().ok_or(Failed)? {
            Session::Serial(session) => session
                .process_uninit(input, output, self.operation)
                .map(Outcome::from)
                .map_err(|err| err.to_string()),
            Session::Dictionary(session) => session
                .process_uninit(input, output, self.operation)
                .map(Outcome::from)
                .map_err(|err| err.to_string()),
            Session::Parallel { session, .. } => session
                .process_uninit(input, output, self.operation)
                .map(Outcome::from)
                .map_err(|err| err.to_string()),
        };
        let progress = outcome.map_err(|err| self.encode_error(&err))?;

        if progress.consumed > 0 {
            unsafe { (*self.in_buf).pos = (*self.in_buf).pos.add(progress.consumed) };
            if progress.consumed == input_len {
                self.in_buf = ptr::null_mut();
            }
        }
        let out_full = unsafe {
            (*out_buf).last = last.add(progress.produced);
            (*out_buf).last == (*out_buf).end
        };
        self.bytes_in += progress.consumed as u64;
        self.bytes_out += progress.produced as u64;

        self.host.debug(format_args!(
            "zstd in:{} consumed:{} out:{} produced:{} op:{:?} status:{:?} tasks:{}",
            input_len,
            progress.consumed,
            room,
            progress.produced,
            self.operation,
            progress.status,
            self.in_flight
        ));

        let ran_inline = self.post_tasks();

        if out_full && progress.status != Status::Finished {
            self.queue_out_buf()?;
        }

        match progress.status {
            Status::NeedsOutput => {
                if progress.produced == 0 {
                    self.host
                        .error(format_args!("zstd encoder made no progress"));
                    return Err(Failed);
                }
                self.redo = true;
                Ok(Step::Again)
            }

            Status::Pending => {
                self.redo = false;
                if ran_inline {
                    // Those tasks are complete already.
                    self.collect = true;
                    return Ok(Step::Again);
                }
                if self.in_flight == 0 {
                    // Nothing would call the filter again.
                    self.host
                        .error(format_args!("zstd encoder waits for no task"));
                    return Err(Failed);
                }
                // Called again once a task completes.
                Ok(Step::Ok)
            }

            Status::Finished => {
                self.redo = false;
                let b = self.take_out_buf();
                if b.is_null() {
                    return Err(Failed);
                }
                unsafe { self.host.set_last_buf(b) };
                self.queue(b)?;

                self.done = true;
                self.success = true;
                self.release_encoder();
                self.set_buffered(false);
                Ok(Step::Ok)
            }

            Status::NeedsInput => {
                self.redo = false;
                match self.operation {
                    Operation::Continue => {
                        if progress.consumed == 0 && input_len != 0 {
                            self.host
                                .error(format_args!("zstd encoder made no progress"));
                            return Err(Failed);
                        }
                        Ok(Step::Again)
                    }

                    Operation::Flush => {
                        self.operation = Operation::Continue;

                        let b = match unsafe { self.out_buf.as_ref() } {
                            Some(b) if b.last != b.pos => self.take_out_buf(),
                            // Nothing to send: a special buffer carries the flush.
                            _ => self.host.calloc_buf(),
                        };
                        if b.is_null() {
                            return Err(Failed);
                        }
                        unsafe { self.host.set_flush(b) };
                        self.queue(b)?;

                        self.set_buffered(false);
                        Ok(Step::Ok)
                    }

                    Operation::End => {
                        self.host
                            .error(format_args!("zstd encoder did not finish the stream"));
                        Err(Failed)
                    }
                }
            }
        }
    }

    /// Posts the tasks a parallel session has queued to the thread pool.
    /// Returns whether any had to run here instead.
    fn post_tasks(&mut self) -> bool {
        let Some(Session::Parallel { queue, .. }) = &self.session else {
            return false;
        };

        let mut ran_inline = false;
        while let Some(task) = queue.next_task() {
            match self.host.post_task(task) {
                None => {
                    if self.in_flight == 0 {
                        self.host.set_blocked(true);
                    }
                    self.in_flight += 1;
                }
                // The thread pool's queue is full: better slow than failed.
                Some(task) => {
                    task.run();
                    ran_inline = true;
                }
            }
        }
        ran_inline
    }

    /// Detaches the current output buffer, allocating an empty one if needed.
    fn take_out_buf(&mut self) -> *mut ngx_buf_t {
        let b = core::mem::replace(&mut self.out_buf, ptr::null_mut());
        if b.is_null() {
            self.host.calloc_buf()
        } else {
            b
        }
    }

    fn queue_out_buf(&mut self) -> Result<(), Failed> {
        let b = self.take_out_buf();
        self.queue(b)
    }

    fn queue(&mut self, b: *mut ngx_buf_t) -> Result<(), Failed> {
        let cl = self.host.alloc_chain_link();
        if b.is_null() || cl.is_null() {
            return Err(Failed);
        }

        unsafe {
            (*cl).buf = b;
            (*cl).next = ptr::null_mut();
            match self.out_tail.as_mut() {
                Some(tail) => tail.next = cl,
                None => self.out = cl,
            }
        }
        self.out_tail = cl;

        Ok(())
    }
}

#[cfg(test)]
mod tests;
