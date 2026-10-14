//! `ngx_http_zstd_filter_module`: on-the-fly Zstandard compression of
//! responses.
//!
//! Directives and behaviour follow tokers/zstd-nginx-module; output buffering
//! follows the stock gzip filter.
//!
//! With `zstd_thread_pool`, responses are encoded in segments on that NGINX
//! thread pool (see [`threads`]).

use core::ffi::{c_char, c_void};
use core::{fmt, ptr};

use mzstd::compressor::parallel::{OwnedParallelTask, SegmentSize, TaskCount};
use mzstd::{CompressionParams, EncoderConfig, InputSize, Level};
use ngx::core::{NGX_CONF_ERROR, NGX_CONF_OK, Pool};
use ngx::ffi::{
    NGX_CONF_1MORE, NGX_CONF_FLAG, NGX_CONF_TAKE1, NGX_CONF_TAKE2, NGX_CONF_TAKE12, NGX_ERROR,
    NGX_HTTP_FORBIDDEN, NGX_HTTP_GZIP_BUFFERED, NGX_HTTP_LIF_CONF, NGX_HTTP_LOC_CONF,
    NGX_HTTP_LOC_CONF_OFFSET, NGX_HTTP_MAIN_CONF, NGX_HTTP_MAIN_CONF_OFFSET, NGX_HTTP_MODULE,
    NGX_HTTP_NOT_FOUND, NGX_HTTP_OK, NGX_HTTP_SRV_CONF, NGX_HTTP_VAR_NOCACHEABLE, NGX_LOG_ALERT,
    NGX_LOG_EMERG, NGX_LOG_WARN, NGX_OK, ngx_alloc_chain_link, ngx_array_t, ngx_buf_t, ngx_bufs_t,
    ngx_chain_add_copy, ngx_chain_t, ngx_chain_update_chains, ngx_command_t,
    ngx_conf_check_num_bounds, ngx_conf_num_bounds_t, ngx_conf_post_t, ngx_conf_set_bufs_slot,
    ngx_conf_set_flag_slot, ngx_conf_set_num_slot, ngx_conf_set_size_slot, ngx_conf_set_str_slot,
    ngx_conf_t, ngx_create_temp_buf, ngx_cycle_t, ngx_flag_t, ngx_hash_t, ngx_http_add_variable,
    ngx_http_conf_ctx_t, ngx_http_html_default_types, ngx_http_merge_types, ngx_http_module,
    ngx_http_module_t, ngx_http_output_body_filter_pt, ngx_http_output_header_filter_pt,
    ngx_http_request_t, ngx_http_test_content_type, ngx_http_top_body_filter,
    ngx_http_top_header_filter, ngx_http_types_slot, ngx_http_variable_value_t, ngx_int_t,
    ngx_list_push, ngx_log_t, ngx_module_t, ngx_pagesize, ngx_palloc, ngx_parse_size, ngx_pcalloc,
    ngx_pnalloc, ngx_pool_t, ngx_str_t, ngx_table_elt_t, ngx_uint_t, off_t,
};
use ngx::{ngx_conf_log_error, ngx_log_debug_mask, ngx_log_error, ngx_string};

use crate::accept_encoding;
use crate::bitfields::{
    buf_flags, ngx_mzstd_buf_set_flush, ngx_mzstd_buf_set_last_buf, ngx_mzstd_buf_set_recycled,
    ngx_mzstd_r_header_only, ngx_mzstd_r_output_pending, ngx_mzstd_r_prepare_encoded,
    ngx_mzstd_r_set_buffered, ngx_mzstd_r_set_gzip_vary, ngx_mzstd_var_set,
    ngx_mzstd_var_set_not_found,
};
use crate::body::{BodyFilter, BufFlags, Host, Parallel, Settings};
use crate::encoder_pool::{self, RetentionPolicy};
use crate::ratio;

/// The gzip and zstd filters never both encode a response, so they can share
/// the "buffered" bit, exactly as tokers/zstd-nginx-module does.
const ZSTD_BUFFERED: u32 = NGX_HTTP_GZIP_BUFFERED;

/// `zstd_window` from 1k to 8m: RFC 9659 limits the window of the `zstd`
/// content coding to 8 MB, and browsers reject frames that need more.
const MIN_WINDOW_LOG: usize = 10;
const MAX_WINDOW_LOG: usize = 23;

/// The window log of each level for long responses (levels 1 to 22; index 0
/// is every negative level), from zstd's table of default parameters.
const LEVEL_WINDOW_LOG: [u8; 23] = [
    19, 19, 20, 21, 21, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 25, 26, 27,
];

const UNSET: isize = -1;
/// `zstd_comp_level` takes negative levels, so -1 cannot mean unset.
const UNSET_LEVEL: ngx_int_t = ngx_int_t::MIN;
const UNSET_SIZE: usize = usize::MAX;
/// `NGX_CONF_UNSET_PTR`.
const UNSET_PTR: *mut c_void = usize::MAX as *mut c_void;

const MIN_SEGMENT_SIZE: usize = 64 << 10;
const MAX_SEGMENT_SIZE: usize = 16 << 20;
const MAX_PARALLEL_TASKS: ngx_int_t = 1024;
const MAX_CACHED: ngx_int_t = 1024;

#[cfg(ngx_feature = "threads")]
mod threads;

/// Main configuration: the limits of each worker's encoder caches.
#[repr(C)]
struct FilterMainConf {
    /// `zstd_idle_encoders`.
    idle_encoders: ngx_int_t,
    /// `zstd_parallel_compressors`.
    parallel_compressors: ngx_int_t,
    /// `zstd_dict_file`, accepted and ignored.
    dict_file: ngx_str_t,
}

/// Location configuration. Field offsets are used by the stock NGINX slot
/// setters, so the layout must stay `repr(C)`.
#[repr(C)]
struct FilterConf {
    enable: ngx_flag_t,
    types: ngx_hash_t,
    types_keys: *mut ngx_array_t,
    min_length: isize,
    bufs: ngx_bufs_t,
    level: ngx_int_t,
    lg_win: usize,
    reuse_memory: ngx_flag_t,
    /// `None` until `zstd_retention_policy` is set or merged.
    retention: Option<RetentionPolicy>,
    /// The `ngx_thread_pool_t` of `zstd_thread_pool`; null when it is off.
    thread_pool: *mut c_void,
    segment_size: usize,
    parallel_tasks: ngx_int_t,
}

static mut WINDOW_POST: ngx_conf_post_t = ngx_conf_post_t {
    post_handler: Some(parse_window_bits),
};

static mut SEGMENT_SIZE_POST: ngx_conf_post_t = ngx_conf_post_t {
    post_handler: Some(check_segment_size),
};

static mut CACHED_BOUNDS: ngx_conf_num_bounds_t = ngx_conf_num_bounds_t {
    post_handler: Some(ngx_conf_check_num_bounds),
    low: 0,
    high: MAX_CACHED,
};

static mut PARALLEL_TASKS_BOUNDS: ngx_conf_num_bounds_t = ngx_conf_num_bounds_t {
    post_handler: Some(ngx_conf_check_num_bounds),
    low: 1,
    high: MAX_PARALLEL_TASKS,
};

static mut DICT_FILE_POST: ngx_conf_post_t = ngx_conf_post_t {
    post_handler: Some(ignore_dict_file),
};

const ANY_CONF: u32 = NGX_HTTP_MAIN_CONF | NGX_HTTP_SRV_CONF | NGX_HTTP_LOC_CONF;

static mut COMMANDS: [ngx_command_t; 15] = [
    ngx_command_t {
        name: ngx_string!("zstd"),
        type_: (ANY_CONF | NGX_HTTP_LIF_CONF | NGX_CONF_FLAG) as ngx_uint_t,
        set: Some(ngx_conf_set_flag_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, enable),
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("zstd_buffers"),
        type_: (ANY_CONF | NGX_CONF_TAKE2) as ngx_uint_t,
        set: Some(ngx_conf_set_bufs_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, bufs),
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("zstd_types"),
        type_: (ANY_CONF | NGX_CONF_1MORE) as ngx_uint_t,
        set: Some(ngx_http_types_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, types_keys),
        post: &raw mut ngx_http_html_default_types as *mut c_void,
    },
    ngx_command_t {
        name: ngx_string!("zstd_comp_level"),
        type_: (ANY_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(set_comp_level),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, level),
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("zstd_window"),
        type_: (ANY_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_conf_set_size_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, lg_win),
        post: &raw mut WINDOW_POST as *mut c_void,
    },
    ngx_command_t {
        name: ngx_string!("zstd_min_length"),
        // tokers/zstd-nginx-module accepts and ignores further arguments.
        type_: (ANY_CONF | NGX_CONF_1MORE) as ngx_uint_t,
        set: Some(ngx_conf_set_size_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, min_length),
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("zstd_reuse_memory"),
        type_: (ANY_CONF | NGX_CONF_FLAG) as ngx_uint_t,
        set: Some(ngx_conf_set_flag_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, reuse_memory),
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("zstd_retention_policy"),
        type_: (ANY_CONF | NGX_CONF_TAKE12) as ngx_uint_t,
        set: Some(set_retention_policy),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: 0,
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("zstd_thread_pool"),
        type_: (ANY_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(set_thread_pool),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: 0,
        post: ptr::null_mut(),
    },
    ngx_command_t {
        name: ngx_string!("zstd_parallel_segment_size"),
        type_: (ANY_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_conf_set_size_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, segment_size),
        post: &raw mut SEGMENT_SIZE_POST as *mut c_void,
    },
    ngx_command_t {
        name: ngx_string!("zstd_parallel_tasks"),
        type_: (ANY_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_conf_set_num_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterConf, parallel_tasks),
        post: &raw mut PARALLEL_TASKS_BOUNDS as *mut c_void,
    },
    ngx_command_t {
        name: ngx_string!("zstd_idle_encoders"),
        type_: (NGX_HTTP_MAIN_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_conf_set_num_slot),
        conf: NGX_HTTP_MAIN_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterMainConf, idle_encoders),
        post: &raw mut CACHED_BOUNDS as *mut c_void,
    },
    ngx_command_t {
        name: ngx_string!("zstd_parallel_compressors"),
        type_: (NGX_HTTP_MAIN_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(set_parallel_compressors),
        conf: NGX_HTTP_MAIN_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterMainConf, parallel_compressors),
        post: &raw mut CACHED_BOUNDS as *mut c_void,
    },
    ngx_command_t {
        name: ngx_string!("zstd_dict_file"),
        type_: (NGX_HTTP_MAIN_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_conf_set_str_slot),
        conf: NGX_HTTP_MAIN_CONF_OFFSET,
        offset: core::mem::offset_of!(FilterMainConf, dict_file),
        post: &raw mut DICT_FILE_POST as *mut c_void,
    },
    ngx_command_t::empty(),
];

static MODULE_CTX: ngx_http_module_t = ngx_http_module_t {
    preconfiguration: Some(add_variables),
    postconfiguration: Some(filter_init),
    create_main_conf: Some(create_main_conf),
    init_main_conf: Some(init_main_conf),
    create_srv_conf: None,
    merge_srv_conf: None,
    create_loc_conf: Some(create_conf),
    merge_loc_conf: Some(merge_conf),
};

#[used]
#[allow(non_upper_case_globals)]
#[cfg_attr(not(feature = "export-modules"), unsafe(no_mangle))]
pub static mut ngx_http_zstd_filter_module: ngx_module_t = ngx_module_t {
    ctx: &raw const MODULE_CTX as _,
    commands: unsafe { &raw mut COMMANDS[0] },
    type_: NGX_HTTP_MODULE as _,
    init_process: Some(init_process),
    ..ngx_module_t::default()
};

static mut NEXT_HEADER_FILTER: ngx_http_output_header_filter_pt = None;
static mut NEXT_BODY_FILTER: ngx_http_output_body_filter_pt = None;

fn module() -> &'static ngx_module_t {
    unsafe { &*ptr::addr_of!(ngx_http_zstd_filter_module) }
}

/// Tags the output buffers this module owns, for `ngx_chain_update_chains`.
fn buf_tag() -> *mut c_void {
    (&raw mut ngx_http_zstd_filter_module).cast()
}

unsafe fn next_header_filter(r: *mut ngx_http_request_t) -> ngx_int_t {
    let next = unsafe { NEXT_HEADER_FILTER }.expect("zstd filter is initialized");
    unsafe { next(r) }
}

unsafe fn next_body_filter(r: *mut ngx_http_request_t, chain: *mut ngx_chain_t) -> ngx_int_t {
    let next = unsafe { NEXT_BODY_FILTER }.expect("zstd filter is initialized");
    unsafe { next(r, chain) }
}

unsafe fn loc_conf<'a>(r: *mut ngx_http_request_t) -> &'a FilterConf {
    unsafe { &*(*(*r).loc_conf.add(module().ctx_index)).cast::<FilterConf>() }
}

type FilterCtx = BodyFilter<NginxHost>;

/// The filter state of `r`; null when its response is not compressed.
unsafe fn request_ctx(r: *mut ngx_http_request_t) -> *mut FilterCtx {
    unsafe { (*(*r).ctx.add(module().ctx_index)).cast() }
}

/// Header filter: decides whether the response is compressed.
unsafe extern "C" fn header_filter(r: *mut ngx_http_request_t) -> ngx_int_t {
    let req = unsafe { &mut *r };
    let conf = unsafe { loc_conf(r) };

    if conf.enable == 0 {
        return unsafe { next_header_filter(r) };
    }

    let status = req.headers_out.status;
    if status != NGX_HTTP_OK as ngx_uint_t
        && status != NGX_HTTP_FORBIDDEN as ngx_uint_t
        && status != NGX_HTTP_NOT_FOUND as ngx_uint_t
    {
        return unsafe { next_header_filter(r) };
    }

    if unsafe { ngx_mzstd_r_header_only(r) } != 0 {
        return unsafe { next_header_filter(r) };
    }

    if let Some(encoding) = unsafe { req.headers_out.content_encoding.as_ref() }
        && encoding.value.len != 0
    {
        return unsafe { next_header_filter(r) };
    }

    let content_length = req.headers_out.content_length_n;
    if content_length != -1 && content_length < conf.min_length as off_t {
        return unsafe { next_header_filter(r) };
    }

    if unsafe { ngx_http_test_content_type(r, &conf.types as *const _ as *mut _) }.is_null() {
        return unsafe { next_header_filter(r) };
    }

    unsafe { ngx_mzstd_r_set_gzip_vary(r) };

    if !unsafe { accept_encoding::check_request(r) } {
        return unsafe { next_header_filter(r) };
    }

    let settings = match settings(conf, content_length) {
        Ok(settings) => settings,
        Err(err) => {
            let log = unsafe { (*req.connection).log };
            ngx_log_error!(NGX_LOG_ALERT, log, "zstd encoder config failed: {err}");
            return unsafe { next_header_filter(r) };
        }
    };

    let host = NginxHost {
        request: r,
        thread_pool: conf.thread_pool,
    };
    let ctx = BodyFilter::new(host, settings);
    let ctx = unsafe { Pool::from_ngx_pool(req.pool) }.allocate(ctx);
    if ctx.is_null() {
        return NGX_ERROR as ngx_int_t;
    }
    unsafe { *req.ctx.add(module().ctx_index) = ctx.cast() };

    let h: *mut ngx_table_elt_t = unsafe { ngx_list_push(&mut req.headers_out.headers) }.cast();
    let Some(h) = (unsafe { h.as_mut() }) else {
        return NGX_ERROR as ngx_int_t;
    };
    h.hash = 1;
    h.next = ptr::null_mut();
    h.key = ngx_string!("Content-Encoding");
    h.value = ngx_string!("zstd");
    h.lowcase_key = c"content-encoding".as_ptr().cast_mut().cast();
    req.headers_out.content_encoding = h;

    unsafe { ngx_mzstd_r_prepare_encoded(r) };

    unsafe { next_header_filter(r) }
}

/// Body filter: feeds the response through the encoder.
unsafe extern "C" fn body_filter(r: *mut ngx_http_request_t, input: *mut ngx_chain_t) -> ngx_int_t {
    let ctx = unsafe { request_ctx(r) };

    if ctx.is_null() || unsafe { ngx_mzstd_r_header_only(r) } != 0 {
        return unsafe { next_body_filter(r, input) };
    }

    if unsafe { (*ctx).is_done() } {
        // A complete response stays buffered until its last thread task is
        // reported, and the writer calls the filters without input meanwhile.
        // With nothing left to send, the write filter would fail that call
        // ("the http output chain is empty").
        if input.is_null() && unsafe { ngx_mzstd_r_output_pending(r) } == 0 {
            return NGX_OK as ngx_int_t;
        }
        return unsafe { next_body_filter(r, input) };
    }

    ngx_log_debug_mask!(
        DebugMask::Http,
        unsafe { (*(*r).connection).log },
        "http zstd filter"
    );

    unsafe { (*ctx).filter(input) }
}

/// The NGINX side of [`Host`], for one request.
struct NginxHost {
    request: *mut ngx_http_request_t,
    /// The `ngx_thread_pool_t` segments are posted to, if any.
    thread_pool: *mut c_void,
}

impl NginxHost {
    fn pool(&self) -> *mut ngx_pool_t {
        unsafe { (*self.request).pool }
    }

    fn log(&self) -> *mut ngx_log_t {
        unsafe { (*(*self.request).connection).log }
    }
}

impl Host for NginxHost {
    unsafe fn chain_add_copy(
        &mut self,
        chain: &mut *mut ngx_chain_t,
        input: *mut ngx_chain_t,
    ) -> bool {
        unsafe { ngx_chain_add_copy(self.pool(), chain, input) == NGX_OK as ngx_int_t }
    }

    unsafe fn update_chains(
        &mut self,
        free: &mut *mut ngx_chain_t,
        busy: &mut *mut ngx_chain_t,
        out: &mut *mut ngx_chain_t,
    ) {
        unsafe { ngx_chain_update_chains(self.pool(), free, busy, out, buf_tag()) };
    }

    fn alloc_chain_link(&mut self) -> *mut ngx_chain_t {
        unsafe { ngx_alloc_chain_link(self.pool()) }
    }

    unsafe fn free_chain(&mut self, cl: *mut ngx_chain_t) {
        // ngx_free_chain()
        let pool = self.pool();
        unsafe {
            (*cl).next = (*pool).chain;
            (*pool).chain = cl;
        }
    }

    fn create_temp_buf(&mut self, size: usize) -> *mut ngx_buf_t {
        let b = unsafe { ngx_create_temp_buf(self.pool(), size) };
        if !b.is_null() {
            unsafe {
                (*b).tag = buf_tag();
                ngx_mzstd_buf_set_recycled(b);
            }
        }
        b
    }

    fn calloc_buf(&mut self) -> *mut ngx_buf_t {
        unsafe { ngx_pcalloc(self.pool(), size_of::<ngx_buf_t>()) }.cast()
    }

    unsafe fn next_body_filter(&mut self, chain: *mut ngx_chain_t) -> ngx_int_t {
        unsafe { next_body_filter(self.request, chain) }
    }

    fn set_buffered(&mut self, on: bool) {
        unsafe {
            ngx_mzstd_r_set_buffered(self.request, ZSTD_BUFFERED as ngx_uint_t, on as ngx_uint_t)
        };
    }

    fn post_task(&mut self, task: OwnedParallelTask) -> Option<OwnedParallelTask> {
        if self.thread_pool.is_null() {
            return Some(task);
        }
        #[cfg(ngx_feature = "threads")]
        return unsafe { threads::post(self.request, self.thread_pool.cast(), task) };
        #[cfg(not(ngx_feature = "threads"))]
        Some(task)
    }

    fn set_blocked(&mut self, on: bool) {
        #[cfg(ngx_feature = "threads")]
        unsafe {
            crate::bitfields::ngx_mzstd_r_set_blocked(self.request, on as ngx_uint_t)
        };
        #[cfg(not(ngx_feature = "threads"))]
        let _ = on;
    }

    unsafe fn buf_flags(&self, b: *const ngx_buf_t) -> BufFlags {
        unsafe { buf_flags(b.cast_mut()) }
    }

    unsafe fn set_flush(&self, b: *mut ngx_buf_t) {
        unsafe { ngx_mzstd_buf_set_flush(b) };
    }

    unsafe fn set_last_buf(&self, b: *mut ngx_buf_t) {
        unsafe { ngx_mzstd_buf_set_last_buf(b) };
    }

    fn error(&self, args: fmt::Arguments<'_>) {
        ngx_log_error!(NGX_LOG_ALERT, self.log(), "{args}");
    }

    fn debug(&self, args: fmt::Arguments<'_>) {
        ngx_log_debug_mask!(DebugMask::Http, self.log(), "{args}");
    }
}

/// Resolves the location configuration for a response of `content_length`
/// bytes (`-1` if unknown).
fn settings(
    conf: &FilterConf,
    content_length: off_t,
) -> Result<Settings, Box<dyn core::error::Error>> {
    let level = Level::try_from(i32::try_from(conf.level)?)?;
    let mut encoder = EncoderConfig::default().with_level(level);
    // The window is set only when the level's own is larger, so that every
    // other level compresses exactly as zstd does without one. A known length
    // shrinks it further for a shorter response.
    if conf.lg_win < usize::from(LEVEL_WINDOW_LOG[level.get().max(0) as usize]) {
        encoder =
            encoder.with_params(CompressionParams::default().with_window_log(conf.lg_win as u32));
    }
    let parallel = if conf.thread_pool.is_null() {
        None
    } else {
        Some(Parallel {
            segment_size: SegmentSize::try_from(conf.segment_size)?,
            tasks: TaskCount::try_from(conf.parallel_tasks as usize)?,
        })
    };

    Ok(Settings {
        encoder,
        input_size: if content_length > 0 {
            InputSize::Exact(content_length as u64)
        } else {
            InputSize::Unknown
        },
        reuse_memory: conf.reuse_memory != 0,
        retention: conf.retention.unwrap_or(encoder_pool::DEFAULT_RETENTION),
        bufs_num: conf.bufs.num as usize,
        bufs_size: conf.bufs.size,
        parallel,
    })
}

unsafe extern "C" fn create_main_conf(cf: *mut ngx_conf_t) -> *mut c_void {
    let conf: *mut FilterMainConf =
        unsafe { ngx_palloc((*cf).pool, size_of::<FilterMainConf>()) }.cast();
    if !conf.is_null() {
        unsafe {
            conf.write(FilterMainConf {
                idle_encoders: UNSET,
                parallel_compressors: UNSET,
                dict_file: ngx_str_t::default(),
            })
        };
    }
    conf.cast()
}

unsafe extern "C" fn init_main_conf(_cf: *mut ngx_conf_t, conf: *mut c_void) -> *mut c_char {
    let conf = unsafe { &mut *conf.cast::<FilterMainConf>() };
    if conf.idle_encoders == UNSET {
        conf.idle_encoders = encoder_pool::DEFAULT_IDLE_ENCODERS as ngx_int_t;
    }
    if conf.parallel_compressors == UNSET {
        conf.parallel_compressors = encoder_pool::DEFAULT_PARALLEL_COMPRESSORS as ngx_int_t;
    }
    NGX_CONF_OK
}

/// Sets this worker's cache limits from the `http` block, if there is one.
unsafe extern "C" fn init_process(cycle: *mut ngx_cycle_t) -> ngx_int_t {
    let http = unsafe {
        *(*cycle)
            .conf_ctx
            .add((*ptr::addr_of!(ngx_http_module)).index)
    };
    let Some(http) = (unsafe { http.cast::<ngx_http_conf_ctx_t>().as_ref() }) else {
        return NGX_OK as ngx_int_t;
    };
    let conf = unsafe { &*(*http.main_conf.add(module().ctx_index)).cast::<FilterMainConf>() };
    encoder_pool::configure(
        conf.idle_encoders as usize,
        conf.parallel_compressors as usize,
    );
    NGX_OK as ngx_int_t
}

unsafe extern "C" fn create_conf(cf: *mut ngx_conf_t) -> *mut c_void {
    let conf: *mut FilterConf = unsafe { ngx_palloc((*cf).pool, size_of::<FilterConf>()) }.cast();
    if !conf.is_null() {
        unsafe {
            conf.write(FilterConf {
                enable: UNSET,
                types: core::mem::zeroed(),
                types_keys: ptr::null_mut(),
                min_length: UNSET,
                bufs: ngx_bufs_t { num: 0, size: 0 },
                level: UNSET_LEVEL,
                lg_win: UNSET_SIZE,
                reuse_memory: UNSET,
                retention: None,
                thread_pool: UNSET_PTR,
                segment_size: UNSET_SIZE,
                parallel_tasks: UNSET,
            })
        };
    }
    conf.cast()
}

unsafe extern "C" fn merge_conf(
    cf: *mut ngx_conf_t,
    parent: *mut c_void,
    child: *mut c_void,
) -> *mut c_char {
    let prev = unsafe { &mut *parent.cast::<FilterConf>() };
    let conf = unsafe { &mut *child.cast::<FilterConf>() };

    fn merge<T: Copy + PartialEq>(value: &mut T, prev: T, unset: T, default: T) {
        if *value == unset {
            *value = if prev == unset { default } else { prev };
        }
    }

    merge(&mut conf.enable, prev.enable, UNSET, 0);
    merge(&mut conf.level, prev.level, UNSET_LEVEL, 1);
    merge(&mut conf.lg_win, prev.lg_win, UNSET_SIZE, MAX_WINDOW_LOG);
    merge(&mut conf.min_length, prev.min_length, UNSET, 20);
    merge(&mut conf.reuse_memory, prev.reuse_memory, UNSET, 1);
    conf.retention = conf
        .retention
        .or(prev.retention)
        .or(Some(encoder_pool::DEFAULT_RETENTION));
    merge(
        &mut conf.thread_pool,
        prev.thread_pool,
        UNSET_PTR,
        ptr::null_mut(),
    );
    merge(
        &mut conf.segment_size,
        prev.segment_size,
        UNSET_SIZE,
        1 << 20,
    );
    merge(&mut conf.parallel_tasks, prev.parallel_tasks, UNSET, 4);

    if conf.bufs.num == 0 {
        conf.bufs = if prev.bufs.num != 0 {
            prev.bufs
        } else {
            let pagesize = unsafe { ngx_pagesize };
            ngx_bufs_t {
                num: (128 * 1024 / pagesize) as ngx_int_t,
                size: pagesize,
            }
        };
    }

    let rv = unsafe {
        ngx_http_merge_types(
            cf,
            &mut conf.types_keys,
            &mut conf.types,
            &mut prev.types_keys,
            &mut prev.types,
            (&raw mut ngx_http_html_default_types).cast(),
        )
    };
    if !rv.is_null() {
        return NGX_CONF_ERROR;
    }

    NGX_CONF_OK
}

/// `zstd_retention_policy aggressive | current_config | release_all | bounded <size>`.
unsafe extern "C" fn set_retention_policy(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    let conf = unsafe { &mut *conf.cast::<FilterConf>() };
    if conf.retention.is_some() {
        return c"is duplicate".as_ptr().cast_mut();
    }

    let args: &mut [ngx_str_t] = unsafe { (*(*cf).args).as_slice_mut() };
    let (name, size) = match args {
        [_, name] => (*name, None),
        [_, name, size] => (*name, Some(size)),
        _ => return NGX_CONF_ERROR,
    };

    let policy = match (name.as_bytes(), size) {
        (b"aggressive", None) => RetentionPolicy::Aggressive,
        (b"current_config", None) => RetentionPolicy::CurrentConfig,
        (b"release_all", None) => RetentionPolicy::ReleaseAll,
        (b"bounded", Some(size)) => {
            let max_bytes = unsafe { ngx_parse_size(size) };
            if max_bytes == NGX_ERROR as isize {
                ngx_conf_log_error!(
                    NGX_LOG_EMERG,
                    cf,
                    "invalid size \"{size}\" in \"zstd_retention_policy\" directive"
                );
                return NGX_CONF_ERROR;
            }
            RetentionPolicy::Bounded {
                max_bytes: max_bytes as usize,
            }
        }
        (b"bounded", None) => {
            ngx_conf_log_error!(
                NGX_LOG_EMERG,
                cf,
                "\"bounded\" in \"zstd_retention_policy\" directive requires a size"
            );
            return NGX_CONF_ERROR;
        }
        (b"aggressive" | b"current_config" | b"release_all", Some(_)) => {
            ngx_conf_log_error!(
                NGX_LOG_EMERG,
                cf,
                "\"{name}\" in \"zstd_retention_policy\" directive does not take a size"
            );
            return NGX_CONF_ERROR;
        }
        _ => {
            ngx_conf_log_error!(
                NGX_LOG_EMERG,
                cf,
                "invalid value \"{name}\" in \"zstd_retention_policy\" directive, \
                 it must be \"aggressive\", \"current_config\", \"release_all\" \
                 or \"bounded <size>\""
            );
            return NGX_CONF_ERROR;
        }
    };

    conf.retention = Some(policy);
    NGX_CONF_OK
}

/// `zstd_thread_pool off | <name>`.
unsafe extern "C" fn set_thread_pool(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    let conf = unsafe { &mut *conf.cast::<FilterConf>() };
    if conf.thread_pool != UNSET_PTR {
        return c"is duplicate".as_ptr().cast_mut();
    }

    let args: &mut [ngx_str_t] = unsafe { (*(*cf).args).as_slice_mut() };
    let [_, name] = args else {
        return NGX_CONF_ERROR;
    };

    if name.as_bytes() == b"off" {
        conf.thread_pool = ptr::null_mut();
        return NGX_CONF_OK;
    }

    #[cfg(ngx_feature = "threads")]
    {
        // The pool is looked up, or registered to be checked once the
        // configuration has been read: `thread_pool` may come later.
        let tp = unsafe { ngx::ffi::ngx_thread_pool_add(cf, name) };
        if tp.is_null() {
            return NGX_CONF_ERROR;
        }
        conf.thread_pool = tp.cast();
        NGX_CONF_OK
    }

    #[cfg(not(ngx_feature = "threads"))]
    {
        ngx_conf_log_error!(
            NGX_LOG_EMERG,
            cf,
            "\"zstd_thread_pool\" requires NGINX built with --with-threads"
        );
        NGX_CONF_ERROR
    }
}

/// `zstd_parallel_compressors <number>`, which only NGINX with threads uses.
unsafe extern "C" fn set_parallel_compressors(
    cf: *mut ngx_conf_t,
    cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    #[cfg(ngx_feature = "threads")]
    {
        unsafe { ngx_conf_set_num_slot(cf, cmd, conf) }
    }

    #[cfg(not(ngx_feature = "threads"))]
    {
        let _ = (cmd, conf);
        ngx_conf_log_error!(
            NGX_LOG_EMERG,
            cf,
            "\"zstd_parallel_compressors\" requires NGINX built with --with-threads"
        );
        NGX_CONF_ERROR
    }
}

/// Checks the bounds of `zstd_parallel_segment_size`.
unsafe extern "C" fn check_segment_size(
    _cf: *mut ngx_conf_t,
    _post: *mut c_void,
    data: *mut c_void,
) -> *mut c_char {
    let value = unsafe { *data.cast::<usize>() };
    if (MIN_SEGMENT_SIZE..=MAX_SEGMENT_SIZE).contains(&value) {
        NGX_CONF_OK
    } else {
        c"must be between 64k and 16m".as_ptr().cast_mut()
    }
}

/// Translates `zstd_window` from a size into a window log.
unsafe extern "C" fn parse_window_bits(
    _cf: *mut ngx_conf_t,
    _post: *mut c_void,
    data: *mut c_void,
) -> *mut c_char {
    let value = unsafe { &mut *data.cast::<usize>() };

    match (MIN_WINDOW_LOG..=MAX_WINDOW_LOG).find(|bits| *value == 1 << bits) {
        Some(bits) => {
            *value = bits;
            NGX_CONF_OK
        }
        None => c"must be 1k, 2k, 4k, 8k, 16k, 32k, 64k, 128k, 256k, 512k, 1m, 2m, 4m or 8m"
            .as_ptr()
            .cast_mut(),
    }
}

/// `zstd_comp_level <level>`, from -131072 to 22 except 0.
/// `ngx_conf_set_num_slot` does not take negative numbers.
unsafe extern "C" fn set_comp_level(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    let conf = unsafe { &mut *conf.cast::<FilterConf>() };
    if conf.level != UNSET_LEVEL {
        return c"is duplicate".as_ptr().cast_mut();
    }

    let args: &mut [ngx_str_t] = unsafe { (*(*cf).args).as_slice_mut() };
    let [_, value] = args else {
        return NGX_CONF_ERROR;
    };

    let Some(level) = parse_level(value.as_bytes()) else {
        return c"invalid number".as_ptr().cast_mut();
    };
    if level == 0 || !(Level::MIN.get()..=Level::MAX.get()).contains(&level) {
        return c"must be between -131072 and 22, excluding 0"
            .as_ptr()
            .cast_mut();
    }

    conf.level = level as ngx_int_t;
    NGX_CONF_OK
}

/// Parses an optionally negative decimal number.
fn parse_level(value: &[u8]) -> Option<i32> {
    let (negative, digits) = match value {
        [b'-', digits @ ..] => (true, digits),
        digits => (false, digits),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let magnitude: i32 = core::str::from_utf8(digits).ok()?.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// `zstd_dict_file <path>`: compression with a dictionary is not supported.
/// A response compressed without one decodes with or without the dictionary,
/// so configurations that set it keep working.
unsafe extern "C" fn ignore_dict_file(
    cf: *mut ngx_conf_t,
    _post: *mut c_void,
    _data: *mut c_void,
) -> *mut c_char {
    ngx_conf_log_error!(
        NGX_LOG_WARN,
        cf,
        "\"zstd_dict_file\" is not supported and is ignored: \
         responses are compressed without a dictionary"
    );
    NGX_CONF_OK
}

unsafe extern "C" fn add_variables(cf: *mut ngx_conf_t) -> ngx_int_t {
    let mut name = ngx_string!("zstd_ratio");
    let var =
        unsafe { ngx_http_add_variable(cf, &mut name, NGX_HTTP_VAR_NOCACHEABLE as ngx_uint_t) };
    let Some(var) = (unsafe { var.as_mut() }) else {
        return NGX_ERROR as ngx_int_t;
    };
    var.get_handler = Some(ratio_variable);
    NGX_OK as ngx_int_t
}

/// `$zstd_ratio`: the achieved compression ratio, e.g. `3.254`.
unsafe extern "C" fn ratio_variable(
    r: *mut ngx_http_request_t,
    v: *mut ngx_http_variable_value_t,
    _data: usize,
) -> ngx_int_t {
    let sizes = unsafe { request_ctx(r).as_ref() }.and_then(FilterCtx::sizes);
    let Some((bytes_in, bytes_out)) = sizes.filter(|&(_, out)| out != 0) else {
        unsafe { ngx_mzstd_var_set_not_found(v) };
        return NGX_OK as ngx_int_t;
    };

    let ratio = ratio::format(bytes_in, bytes_out);
    let data: *mut u8 = unsafe { ngx_pnalloc((*r).pool, ratio.len()) }.cast();
    if data.is_null() {
        return NGX_ERROR as ngx_int_t;
    }
    unsafe {
        ptr::copy_nonoverlapping(ratio.as_ptr(), data, ratio.len());
        ngx_mzstd_var_set(v, data, ratio.len());
    }

    NGX_OK as ngx_int_t
}

unsafe extern "C" fn filter_init(_cf: *mut ngx_conf_t) -> ngx_int_t {
    unsafe {
        NEXT_HEADER_FILTER = ngx_http_top_header_filter;
        ngx_http_top_header_filter = Some(header_filter);

        NEXT_BODY_FILTER = ngx_http_top_body_filter;
        ngx_http_top_body_filter = Some(body_filter);
    }
    NGX_OK as ngx_int_t
}
