//! Running a response's segments on an NGINX thread pool (`zstd_thread_pool`).
//!
//! Each segment a parallel session queues becomes an `ngx_thread_task_t`,
//! allocated from the request pool. A thread of the pool encodes it; the
//! completion handler then runs in the worker's event loop, where it reports
//! the completion to the body filter and calls the filter again to deliver
//! what is ready, as `ngx_http_copy_filter_module` does for `aio threads`.
//!
//! The request must outlive its posted tasks. The body filter blocks it
//! (`r->main->blocked`) while any is posted, which defers terminating it, and
//! keeps it buffered, which defers finalizing it.

use core::ffi::c_void;
use core::ptr;

use mzstd::compressor::parallel::OwnedParallelTask;
use ngx::ffi::{
    NGX_ERROR, NGX_OK, ngx_event_t, ngx_http_finalize_request, ngx_http_request_t,
    ngx_http_run_posted_requests, ngx_int_t, ngx_log_t, ngx_thread_pool_t, ngx_thread_task_alloc,
    ngx_thread_task_post,
};
use ngx::ngx_log_debug_mask;

use super::request_ctx;
use crate::bitfields::{ngx_mzstd_r_thread_done, ngx_mzstd_r_thread_resume};

/// What a posted task carries, after its `ngx_thread_task_t`.
struct TaskCtx {
    request: *mut ngx_http_request_t,
    /// Taken by the thread that runs it.
    task: Option<OwnedParallelTask>,
}

/// Posts `task` to `tp` for `r`. Returns it if it cannot be queued.
///
/// # Safety
///
/// `r` must be a request whose response the zstd filter compresses, and
/// `tp` a thread pool of the running cycle.
pub unsafe fn post(
    r: *mut ngx_http_request_t,
    tp: *mut ngx_thread_pool_t,
    task: OwnedParallelTask,
) -> Option<OwnedParallelTask> {
    let t = unsafe { ngx_thread_task_alloc((*r).pool, size_of::<TaskCtx>()) };
    let Some(t) = (unsafe { t.as_mut() }) else {
        return Some(task);
    };

    // The context follows the task, aligned as the task is.
    let ctx = t.ctx.cast::<TaskCtx>();
    unsafe {
        ctx.write(TaskCtx {
            request: r,
            task: Some(task),
        })
    };
    t.handler = Some(run);
    t.event.data = ctx.cast();
    t.event.handler = Some(completed);
    t.event.log = unsafe { (*(*r).connection).log };

    if unsafe { ngx_thread_task_post(tp, t) } != NGX_OK as ngx_int_t {
        // It was not queued, so it is still there.
        return unsafe { (*ctx).task.take() };
    }

    None
}

/// Runs in a thread of the pool.
unsafe extern "C" fn run(data: *mut c_void, _log: *mut ngx_log_t) {
    // Only this thread touches the context until the completion handler runs.
    if let Some(task) = unsafe { (*data.cast::<TaskCtx>()).task.take() } {
        task.run();
    }
}

/// Runs in the event loop once the task has run.
unsafe extern "C" fn completed(ev: *mut ngx_event_t) {
    let r = unsafe { (*(*ev).data.cast::<TaskCtx>()).request };
    let filter = unsafe { &mut *request_ctx(r) };

    ngx_log_debug_mask!(
        DebugMask::Http,
        unsafe { (*(*r).connection).log },
        "http zstd thread task done"
    );

    // This may unblock the request; it is not freed before it is resumed.
    filter.task_done();

    if unsafe { ngx_mzstd_r_thread_done(r) } != 0 {
        // Nothing will be sent: stop the tasks that have not started.
        filter.abandon();
    } else if !filter.is_done()
        // Deliver what is ready. The filters above are not called: what they
        // pass on is already ordered, and they resume on the write event.
        && unsafe { filter.filter(ptr::null_mut()) } == NGX_ERROR as ngx_int_t
    {
        let c = unsafe { (*r).connection };
        unsafe {
            ngx_http_finalize_request(r, NGX_ERROR as ngx_int_t);
            ngx_http_run_posted_requests(c);
        }
        return;
    }

    // Whatever the event loop would do on a write event: send more, let
    // a request waiting on the filter move on, or finalize it.
    unsafe { ngx_mzstd_r_thread_resume(r) };
}
