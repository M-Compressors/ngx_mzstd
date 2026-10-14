//! Per-worker cache of idle [`Compressor`]s.
//!
//! NGINX runs filters on a single thread per worker process, so a thread-local
//! free list lets consecutive responses reuse an encoder's match finder tables,
//! window and staging buffers instead of allocating them for every request
//! (`zstd_reuse_memory`). How many idle compressors a worker keeps is
//! `zstd_idle_encoders`, and how much each keeps allocated is its retention
//! policy (`zstd_retention_policy`).
//!
//! Parallel compressors (`zstd_thread_pool`) are kept in a list of their own:
//! what they keep between responses is the segment encoders their tasks used.
//! A response holds its compressor, encoders included, until it has been sent,
//! also while its segments wait for a thread; so the cache also counts the
//! compressors with encoders that responses hold, and keeps no more of them in
//! all than `zstd_parallel_compressors` allows.
//!
//! Both limits are set once per worker process, by [`configure`].

use core::cell::{Cell, RefCell};

use mzstd::compressor::parallel::{ParallelCompressor, ParallelConfig};
use mzstd::{Compressor, ConfigError, EncoderConfig};

/// The default of `zstd_idle_encoders`: how many idle compressors a worker
/// keeps, in each of its two caches.
pub const DEFAULT_IDLE_ENCODERS: usize = 4;

/// The default of `zstd_parallel_compressors`: how many parallel compressors
/// keep segment encoders, idle or held by a response.
pub const DEFAULT_PARALLEL_COMPRESSORS: usize = 4;

/// The default retention policy: idle compressors keep every buffer.
///
/// A worker holds on to what its largest responses grew into, so none of it
/// is allocated again for later responses.
pub const DEFAULT_RETENTION: RetentionPolicy = RetentionPolicy::Aggressive;

/// How much working memory an idle compressor keeps (`zstd_retention_policy`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RetentionPolicy {
    /// Every buffer.
    Aggressive,
    /// Every buffer, until the compressor is reused with another
    /// configuration.
    CurrentConfig,
    /// Buffers while they take at most `max_bytes` in all.
    Bounded { max_bytes: usize },
    /// Nothing; only the compressor itself is kept.
    ReleaseAll,
}

impl RetentionPolicy {
    /// The policy a compressor applies on its own after every frame.
    fn codec(self) -> mzstd::RetentionPolicy {
        match self {
            Self::Aggressive | Self::CurrentConfig => mzstd::RetentionPolicy::Aggressive,
            Self::Bounded { max_bytes } => mzstd::RetentionPolicy::Bounded { max_bytes },
            Self::ReleaseAll => mzstd::RetentionPolicy::ReleaseAll,
        }
    }
}

/// An idle compressor and the policy it was built for.
struct Idle {
    retention: RetentionPolicy,
    compressor: Compressor,
}

/// An idle parallel compressor.
struct IdleParallel {
    compressor: ParallelCompressor,
    /// Counted in [`PARALLEL_WITH_ENCODERS`].
    counted: bool,
}

thread_local! {
    /// Idle compressors, least recently released first.
    static IDLE: RefCell<Vec<Idle>> = const { RefCell::new(Vec::new()) };

    /// Idle parallel compressors, least recently released first.
    static IDLE_PARALLEL: RefCell<Vec<IdleParallel>> = const { RefCell::new(Vec::new()) };

    /// Parallel compressors from the cache that keep segment encoders, idle
    /// or held by a response.
    static PARALLEL_WITH_ENCODERS: Cell<usize> = const { Cell::new(0) };

    /// `zstd_idle_encoders`.
    static IDLE_LIMIT: Cell<usize> = const { Cell::new(DEFAULT_IDLE_ENCODERS) };

    /// `zstd_parallel_compressors`.
    static PARALLEL_LIMIT: Cell<usize> = const { Cell::new(DEFAULT_PARALLEL_COMPRESSORS) };
}

/// Sets the cache limits of this thread (`zstd_idle_encoders` and
/// `zstd_parallel_compressors`), before it compresses anything.
pub fn configure(idle_encoders: usize, parallel_compressors: usize) {
    IDLE_LIMIT.set(idle_encoders);
    PARALLEL_LIMIT.set(parallel_compressors);
}

/// Builds a compressor that bypasses the cache.
pub fn build(config: EncoderConfig, retention: RetentionPolicy) -> Result<Compressor, ConfigError> {
    Compressor::builder(config)
        .with_retention(retention.codec())
        .build()
}

/// Returns a compressor for `config` and `retention`, preferring an idle one.
///
/// Only idle compressors built for the same policy are reused; one built for
/// another encoder configuration is reconfigured, after giving up its buffers
/// if the policy is [`RetentionPolicy::CurrentConfig`].
pub fn acquire(
    config: EncoderConfig,
    retention: RetentionPolicy,
) -> Result<Compressor, ConfigError> {
    let cached = IDLE.with_borrow_mut(|idle| {
        let same_policy = |i: &Idle| i.retention == retention;
        let index = idle
            .iter()
            .rposition(|i| same_policy(i) && *i.compressor.config() == config)
            .or_else(|| idle.iter().rposition(same_policy))?;
        Some(idle.remove(index).compressor)
    });

    match cached {
        Some(mut compressor) => {
            if *compressor.config() != config {
                if retention == RetentionPolicy::CurrentConfig {
                    compressor.trim(mzstd::RetentionPolicy::ReleaseAll);
                }
                compressor.reconfigure(config)?;
            }
            Ok(compressor)
        }
        None => build(config, retention),
    }
}

/// Returns `compressor`, built for `retention`, to the cache, evicting the
/// least recently released idle compressor when the cache is full.
pub fn release(mut compressor: Compressor, retention: RetentionPolicy) {
    // A frame that did not finish has not applied the policy.
    compressor.trim(retention.codec());

    let limit = IDLE_LIMIT.get();
    if limit == 0 {
        return;
    }
    IDLE.with_borrow_mut(|idle| {
        if idle.len() >= limit {
            idle.remove(0);
        }
        idle.push(Idle {
            retention,
            compressor,
        });
    });
}

/// Builds a parallel compressor that bypasses the cache.
pub fn build_parallel(
    config: EncoderConfig,
    parallel: ParallelConfig,
) -> Result<ParallelCompressor, ConfigError> {
    ParallelCompressor::new(config, parallel)
}

/// A parallel compressor's place among those that keep segment encoders
/// ([`PARALLEL_WITH_ENCODERS`]), given up when it is dropped.
#[must_use]
pub struct ParallelLease {
    counted: bool,
}

impl Drop for ParallelLease {
    fn drop(&mut self) {
        if self.counted {
            PARALLEL_WITH_ENCODERS.set(PARALLEL_WITH_ENCODERS.get() - 1);
        }
    }
}

/// Returns a parallel compressor for `config` and `parallel`, preferring an
/// idle one with the same configuration, which keeps its segment encoders.
/// The lease goes back to [`release_parallel`] with it.
pub fn acquire_parallel(
    config: EncoderConfig,
    parallel: ParallelConfig,
) -> Result<(ParallelCompressor, ParallelLease), ConfigError> {
    let cached = IDLE_PARALLEL.with_borrow_mut(|idle| {
        let index = idle.iter().rposition(|i| {
            *i.compressor.config() == config && *i.compressor.parallel_config() == parallel
        })?;
        Some(idle.remove(index))
    });

    match cached {
        // An idle compressor with encoders is counted already.
        Some(IdleParallel {
            compressor,
            counted,
        }) => Ok((compressor, ParallelLease { counted })),
        None => {
            let compressor = build_parallel(config, parallel)?;
            Ok((compressor, ParallelLease { counted: false }))
        }
    }
}

/// Returns `compressor` to the cache, keeping as much of its segment encoders'
/// memory as `retention` allows, unless as many other compressors keep
/// encoders as `zstd_parallel_compressors` allows.
pub fn release_parallel(
    mut compressor: ParallelCompressor,
    retention: RetentionPolicy,
    mut lease: ParallelLease,
) {
    match retention {
        // The compressor is only reused with its own configuration.
        RetentionPolicy::Aggressive | RetentionPolicy::CurrentConfig => {}
        RetentionPolicy::Bounded { max_bytes } => {
            if compressor.retained_bytes() > max_bytes {
                compressor.release();
            }
        }
        RetentionPolicy::ReleaseAll => compressor.release(),
    }

    let keeps = compressor.retained_bytes() > 0;
    let counted = match (lease.counted, keeps) {
        // Still counted; it moves to the cache below.
        (true, true) => {
            lease.counted = false;
            true
        }
        (true, false) => false,
        (false, true) if PARALLEL_WITH_ENCODERS.get() < PARALLEL_LIMIT.get() => {
            PARALLEL_WITH_ENCODERS.set(PARALLEL_WITH_ENCODERS.get() + 1);
            true
        }
        (false, true) => {
            compressor.release();
            false
        }
        (false, false) => false,
    };
    drop(lease);
    let released = IdleParallel {
        compressor,
        counted,
    };

    let limit = IDLE_LIMIT.get();
    if limit == 0 {
        uncount(&released);
        return;
    }

    IDLE_PARALLEL.with_borrow_mut(|idle| {
        if idle.len() >= limit {
            // Compressors without encoders are cheap to build again: they go
            // first, and one is not kept in place of one with encoders.
            let evicted = match idle.iter().position(|i| !i.counted) {
                Some(index) => idle.remove(index),
                None if !released.counted => return,
                None => idle.remove(0),
            };
            uncount(&evicted);
        }
        idle.push(released);
    });
}

fn uncount(idle: &IdleParallel) {
    if idle.counted {
        PARALLEL_WITH_ENCODERS.set(PARALLEL_WITH_ENCODERS.get() - 1);
    }
}

#[cfg(test)]
mod tests {
    use mzstd::compressor::parallel::{
        ParallelCompressor, ParallelConfig, ParallelEncoderStatus, SegmentSize, TaskCount,
    };
    use mzstd::{EncoderConfig, Level, Operation};

    use super::{
        DEFAULT_IDLE_ENCODERS as MAX_IDLE, DEFAULT_RETENTION, IDLE, IDLE_PARALLEL,
        PARALLEL_WITH_ENCODERS, ParallelLease, RetentionPolicy, acquire, acquire_parallel,
        configure, release, release_parallel,
    };

    fn config(level: i32) -> EncoderConfig {
        EncoderConfig::default().with_level(Level::try_from(level).unwrap())
    }

    fn compress(config: EncoderConfig, retention: RetentionPolicy, input: &[u8]) -> Vec<u8> {
        let mut session = acquire(config, retention)
            .unwrap()
            .into_session(Default::default());
        let mut out = vec![0; 1 << 16];
        let progress = session.process(input, &mut out, Operation::End).unwrap();
        out.truncate(progress.produced);
        release(session.into_compressor(), retention);
        out
    }

    fn idle_policies() -> Vec<RetentionPolicy> {
        IDLE.with_borrow(|idle| idle.iter().map(|i| i.retention).collect())
    }

    #[test]
    fn reused_compressors_produce_identical_streams() {
        let input = b"reuse reuse reuse reuse reuse".repeat(100);
        let l5 = config(5);
        let l19 = config(19);

        let first = compress(l5, DEFAULT_RETENTION, &input);
        // Switching configuration reconfigures an idle compressor.
        let other = compress(l19, DEFAULT_RETENTION, &input);
        assert_eq!(compress(l5, DEFAULT_RETENTION, &input), first);
        assert_eq!(compress(l19, DEFAULT_RETENTION, &input), other);
    }

    #[test]
    fn idle_list_is_bounded() {
        let held: Vec<_> = (0..MAX_IDLE + 3)
            .map(|_| acquire(config(1), DEFAULT_RETENTION).unwrap())
            .collect();
        for compressor in held {
            release(compressor, DEFAULT_RETENTION);
        }

        assert_eq!(idle_policies().len(), MAX_IDLE);
    }

    #[test]
    fn compressors_keep_the_requested_retention_policy() {
        let l1 = config(1);
        release(
            acquire(l1, RetentionPolicy::ReleaseAll).unwrap(),
            RetentionPolicy::ReleaseAll,
        );

        // An idle compressor with another policy is not handed out...
        let aggressive = acquire(l1, RetentionPolicy::Aggressive).unwrap();
        assert_eq!(aggressive.retention(), mzstd::RetentionPolicy::Aggressive);
        assert_eq!(idle_policies(), [RetentionPolicy::ReleaseAll]);

        // ...but the matching one is.
        let release_all = acquire(l1, RetentionPolicy::ReleaseAll).unwrap();
        assert_eq!(release_all.retention(), mzstd::RetentionPolicy::ReleaseAll);
        assert!(idle_policies().is_empty());
    }

    #[test]
    fn current_config_releases_buffers_for_another_configuration() {
        let input = b"current config current config".repeat(100);
        let policy = RetentionPolicy::CurrentConfig;
        compress(config(3), policy, &input);

        let same = acquire(config(3), policy).unwrap();
        assert!(same.retained_bytes() > 0);
        release(same, policy);

        let other = acquire(config(9), policy).unwrap();
        assert_eq!(other.retained_bytes(), 0);
        assert_eq!(*other.config(), config(9));
    }

    #[test]
    fn a_full_cache_evicts_the_least_recently_released() {
        let l1 = config(1);
        release(
            acquire(l1, RetentionPolicy::ReleaseAll).unwrap(),
            RetentionPolicy::ReleaseAll,
        );

        let held: Vec<_> = (0..MAX_IDLE)
            .map(|_| acquire(l1, RetentionPolicy::Aggressive).unwrap())
            .collect();
        for compressor in held {
            release(compressor, RetentionPolicy::Aggressive);
        }

        assert_eq!(idle_policies(), [RetentionPolicy::Aggressive; MAX_IDLE]);
    }

    fn parallel_config() -> ParallelConfig {
        ParallelConfig::default().with_segment_size(SegmentSize::try_from(64 << 10).unwrap())
    }

    /// Takes a parallel compressor from the cache and compresses with it, so
    /// that it keeps a segment encoder.
    fn busy_parallel(config: EncoderConfig) -> (ParallelCompressor, ParallelLease) {
        let (compressor, lease) = acquire_parallel(config, parallel_config()).unwrap();
        let (mut session, queue) = compressor.into_session(Default::default(), TaskCount::ONE);
        let input = b"parallel parallel parallel".repeat(100);
        let mut out = vec![0; 1 << 16];
        let mut input: &[u8] = &input;
        loop {
            let progress = session.process(input, &mut out, Operation::End).unwrap();
            input = &input[progress.consumed..];
            while let Some(task) = queue.next_task() {
                task.run();
            }
            match progress.status {
                ParallelEncoderStatus::Finished => break,
                ParallelEncoderStatus::Pending => session.wait().unwrap(),
                _ => {}
            }
        }
        let compressor = session.into_compressor();
        assert!(compressor.retained_bytes() > 0);
        (compressor, lease)
    }

    fn parallel_with_encoders() -> (usize, usize) {
        let idle = IDLE_PARALLEL.with_borrow(|idle| idle.iter().filter(|i| i.counted).count());
        (PARALLEL_WITH_ENCODERS.get(), idle)
    }

    #[test]
    fn encoders_are_kept_for_a_bounded_number_of_parallel_compressors() {
        let l5 = config(5);
        let held: Vec<_> = (0..MAX_IDLE + 2).map(|_| busy_parallel(l5)).collect();
        for (compressor, lease) in held {
            release_parallel(compressor, RetentionPolicy::Aggressive, lease);
        }
        assert_eq!(parallel_with_encoders(), (MAX_IDLE, MAX_IDLE));

        // Taken again, they are still counted; dropped, they are not.
        let mut held: Vec<_> = (0..MAX_IDLE + 2).map(|_| busy_parallel(l5)).collect();
        assert_eq!(parallel_with_encoders(), (MAX_IDLE, 0));
        for (compressor, lease) in held.drain(..2) {
            release_parallel(compressor, RetentionPolicy::ReleaseAll, lease);
        }
        assert_eq!(parallel_with_encoders(), (MAX_IDLE - 2, 0));
        drop(busy_parallel(l5));
        assert_eq!(parallel_with_encoders(), (MAX_IDLE - 2, 0));
        drop(held);
        assert_eq!(parallel_with_encoders(), (0, 0));
    }

    #[test]
    fn bounded_retention_releases_large_encoders() {
        let l5 = config(5);
        let (compressor, lease) = busy_parallel(l5);
        let retained = compressor.retained_bytes();
        release_parallel(
            compressor,
            RetentionPolicy::Bounded {
                max_bytes: retained - 1,
            },
            lease,
        );
        assert_eq!(parallel_with_encoders(), (0, 0));

        let (compressor, lease) = busy_parallel(l5);
        let retained = compressor.retained_bytes();
        release_parallel(
            compressor,
            RetentionPolicy::Bounded {
                max_bytes: retained,
            },
            lease,
        );
        assert_eq!(parallel_with_encoders(), (1, 1));
    }

    #[test]
    fn the_limits_are_configurable() {
        let l1 = config(1);
        let l5 = config(5);

        configure(2, 1);
        let held: Vec<_> = (0..5)
            .map(|_| acquire(l1, DEFAULT_RETENTION).unwrap())
            .collect();
        for compressor in held {
            release(compressor, DEFAULT_RETENTION);
        }
        assert_eq!(idle_policies().len(), 2);

        let held: Vec<_> = (0..3).map(|_| busy_parallel(l5)).collect();
        for (compressor, lease) in held {
            release_parallel(compressor, RetentionPolicy::Aggressive, lease);
        }
        assert_eq!(parallel_with_encoders(), (1, 1));
        assert_eq!(IDLE_PARALLEL.with_borrow(Vec::len), 2);

        // Nothing is kept, and nothing stays counted.
        configure(0, 1);
        let (compressor, lease) = busy_parallel(l5);
        release_parallel(compressor, RetentionPolicy::Aggressive, lease);
        release(acquire(l1, DEFAULT_RETENTION).unwrap(), DEFAULT_RETENTION);
        assert_eq!(idle_policies().len(), 1);
        assert_eq!(parallel_with_encoders(), (1, 1));
        IDLE_PARALLEL.with_borrow_mut(Vec::clear);
        PARALLEL_WITH_ENCODERS.set(0);
        let (compressor, lease) = busy_parallel(l5);
        release_parallel(compressor, RetentionPolicy::Aggressive, lease);
        assert_eq!(parallel_with_encoders(), (0, 0));
    }
}
