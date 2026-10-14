//! `ngx_http_zstd_static_module`: serves pre-compressed `.zst` files.

use core::ffi::{c_char, c_void};
use core::ptr;

use ngx::core::{NGX_CONF_OK, Pool};
use ngx::ffi::{
    NGX_CONF_TAKE1, NGX_DECLINED, NGX_EACCES, NGX_ELOOP, NGX_EMLINK, NGX_ENAMETOOLONG, NGX_ENOENT,
    NGX_ENOTDIR, NGX_ERROR, NGX_HTTP_GET, NGX_HTTP_HEAD, NGX_HTTP_INTERNAL_SERVER_ERROR,
    NGX_HTTP_LOC_CONF, NGX_HTTP_LOC_CONF_OFFSET, NGX_HTTP_MAIN_CONF, NGX_HTTP_MODULE,
    NGX_HTTP_NOT_FOUND, NGX_HTTP_OK, NGX_HTTP_SRV_CONF, NGX_LOG_CRIT, NGX_LOG_ERR, NGX_OK,
    ngx_array_push, ngx_buf_t, ngx_chain_t, ngx_command_t, ngx_conf_enum_t, ngx_conf_set_enum_slot,
    ngx_conf_t, ngx_file_t, ngx_http_core_loc_conf_t, ngx_http_core_main_conf_t,
    ngx_http_core_module, ngx_http_discard_request_body, ngx_http_handler_pt,
    ngx_http_map_uri_to_path, ngx_http_module_t, ngx_http_output_filter,
    ngx_http_phases_NGX_HTTP_CONTENT_PHASE, ngx_http_request_t, ngx_http_send_header,
    ngx_http_set_content_type, ngx_http_set_disable_symlinks, ngx_http_set_etag, ngx_int_t,
    ngx_list_push, ngx_module_t, ngx_open_cached_file, ngx_open_file_info_t, ngx_pcalloc,
    ngx_str_t, ngx_table_elt_t, ngx_uint_t,
};
use ngx::http::HttpModuleConfExt;
use ngx::{ngx_log_debug_mask, ngx_log_error, ngx_string};

use crate::accept_encoding;
use crate::bitfields::{
    OpenFileFlags, ngx_mzstd_buf_set_file, ngx_mzstd_clcf_gzip_vary, ngx_mzstd_of_init,
    ngx_mzstd_r_header_only, ngx_mzstd_r_set_gzip_vary, ngx_mzstd_r_set_root_tested,
};

const STATIC_OFF: ngx_uint_t = 0;
const STATIC_ON: ngx_uint_t = 1;
const STATIC_ALWAYS: ngx_uint_t = 2;

const UNSET_UINT: ngx_uint_t = ngx_uint_t::MAX;

const SUFFIX: &[u8] = b".zst\0";

#[repr(C)]
struct StaticConf {
    enable: ngx_uint_t,
}

static mut STATIC_ENUM: [ngx_conf_enum_t; 4] = [
    ngx_conf_enum_t {
        name: ngx_string!("off"),
        value: STATIC_OFF,
    },
    ngx_conf_enum_t {
        name: ngx_string!("on"),
        value: STATIC_ON,
    },
    ngx_conf_enum_t {
        name: ngx_string!("always"),
        value: STATIC_ALWAYS,
    },
    ngx_conf_enum_t {
        name: ngx_str_t {
            len: 0,
            data: ptr::null_mut(),
        },
        value: 0,
    },
];

static mut COMMANDS: [ngx_command_t; 2] = [
    ngx_command_t {
        name: ngx_string!("zstd_static"),
        type_: (NGX_HTTP_MAIN_CONF | NGX_HTTP_SRV_CONF | NGX_HTTP_LOC_CONF | NGX_CONF_TAKE1)
            as ngx_uint_t,
        set: Some(ngx_conf_set_enum_slot),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: core::mem::offset_of!(StaticConf, enable),
        post: &raw mut STATIC_ENUM as *mut c_void,
    },
    ngx_command_t::empty(),
];

static MODULE_CTX: ngx_http_module_t = ngx_http_module_t {
    preconfiguration: None,
    postconfiguration: Some(init),
    create_main_conf: None,
    init_main_conf: None,
    create_srv_conf: None,
    merge_srv_conf: None,
    create_loc_conf: Some(create_conf),
    merge_loc_conf: Some(merge_conf),
};

#[used]
#[allow(non_upper_case_globals)]
#[cfg_attr(not(feature = "export-modules"), unsafe(no_mangle))]
pub static mut ngx_http_zstd_static_module: ngx_module_t = ngx_module_t {
    ctx: &raw const MODULE_CTX as _,
    commands: unsafe { &raw mut COMMANDS[0] },
    type_: NGX_HTTP_MODULE as _,
    ..ngx_module_t::default()
};

fn module() -> &'static ngx_module_t {
    unsafe { &*ptr::addr_of!(ngx_http_zstd_static_module) }
}

unsafe extern "C" fn handler(r: *mut ngx_http_request_t) -> ngx_int_t {
    let req = unsafe { &mut *r };

    if req.method & (NGX_HTTP_GET | NGX_HTTP_HEAD) as ngx_uint_t == 0 {
        return NGX_DECLINED as ngx_int_t;
    }

    if req.uri.as_bytes().last().is_none_or(|&c| c == b'/') {
        return NGX_DECLINED as ngx_int_t;
    }

    let conf = unsafe { &*(*req.loc_conf.add(module().ctx_index)).cast::<StaticConf>() };

    let accepted = match conf.enable {
        STATIC_OFF => return NGX_DECLINED as ngx_int_t,
        // Ignore request properties such as Accept-Encoding.
        STATIC_ALWAYS => true,
        _ => unsafe { accept_encoding::check_request(r) },
    };

    let Some(clcf) = (unsafe {
        req.http_location_conf_unchecked::<ngx_http_core_loc_conf_t>(&*ptr::addr_of!(
            ngx_http_core_module
        ))
    }) else {
        return NGX_HTTP_INTERNAL_SERVER_ERROR as ngx_int_t;
    };
    let clcf = clcf.as_ptr();

    // With `gzip_vary`, a response the client does not accept still varies
    // on Accept-Encoding if the file exists, as with tokers/zstd-nginx-module
    // and `gzip_static`.
    if !accepted && unsafe { ngx_mzstd_clcf_gzip_vary(clcf) } == 0 {
        return NGX_DECLINED as ngx_int_t;
    }

    match unsafe { send_file(r, clcf, conf.enable, accepted) } {
        Ok(rc) | Err(rc) => rc,
    }
}

/// Sends `<path>.zst` if it exists and the client accepts it. `Err` carries
/// the status to return early.
unsafe fn send_file(
    r: *mut ngx_http_request_t,
    clcf: *mut ngx_http_core_loc_conf_t,
    enable: ngx_uint_t,
    accepted: bool,
) -> Result<ngx_int_t, ngx_int_t> {
    const SERVER_ERROR: ngx_int_t = NGX_HTTP_INTERNAL_SERVER_ERROR as ngx_int_t;
    const DECLINED: ngx_int_t = NGX_DECLINED as ngx_int_t;

    let req = unsafe { &mut *r };
    let log = unsafe { (*req.connection).log };

    let mut path = ngx_str_t::default();
    let mut root = 0;
    let last = unsafe { ngx_http_map_uri_to_path(r, &mut path, &mut root, SUFFIX.len() - 1) };
    if last.is_null() {
        return Err(SERVER_ERROR);
    }
    unsafe { ptr::copy_nonoverlapping(SUFFIX.as_ptr(), last, SUFFIX.len()) };
    // `path.len` is the size of the allocation, not of the string in it; the
    // name ends right before the terminating NUL written above.
    path.len = unsafe { last.add(SUFFIX.len() - 1).offset_from(path.data) } as usize;

    ngx_log_debug_mask!(DebugMask::Http, log, "http filename: \"{path}\"");

    let mut of: ngx_open_file_info_t = unsafe { core::mem::zeroed() };
    unsafe { ngx_mzstd_of_init(&mut of, clcf) };

    if unsafe { ngx_http_set_disable_symlinks(r, clcf, &mut path, &mut of) } != NGX_OK as ngx_int_t
    {
        return Err(SERVER_ERROR);
    }

    if unsafe { ngx_open_cached_file((*clcf).open_file_cache, &mut path, &mut of, req.pool) }
        != NGX_OK as ngx_int_t
    {
        let level = match of.err as u32 {
            0 => return Err(SERVER_ERROR),
            NGX_ENOENT | NGX_ENOTDIR | NGX_ENAMETOOLONG => return Err(DECLINED),
            NGX_EMLINK | NGX_ELOOP | NGX_EACCES => NGX_LOG_ERR,
            _ => NGX_LOG_CRIT,
        };
        let failed = unsafe { core::ffi::CStr::from_ptr(of.failed) }.to_string_lossy();
        let err = std::io::Error::from_raw_os_error(of.err as i32);
        ngx_log_error!(level, log, "{failed} \"{path}\" failed ({err})");
        return Err(DECLINED);
    }

    if enable == STATIC_ON {
        unsafe { ngx_mzstd_r_set_gzip_vary(r) };
        if !accepted {
            return Err(DECLINED);
        }
    }

    ngx_log_debug_mask!(DebugMask::Http, log, "http static fd: {}", of.fd);

    let flags = unsafe { OpenFileFlags::of(&mut of) };

    if flags.is_dir() {
        ngx_log_debug_mask!(DebugMask::Http, log, "http dir");
        return Err(DECLINED);
    }

    if !flags.is_file() {
        ngx_log_error!(NGX_LOG_CRIT, log, "\"{path}\" is not a regular file");
        return Err(NGX_HTTP_NOT_FOUND as ngx_int_t);
    }

    unsafe { ngx_mzstd_r_set_root_tested(r) };

    let rc = unsafe { ngx_http_discard_request_body(r) };
    if rc != NGX_OK as ngx_int_t {
        return Err(rc);
    }

    unsafe { (*log).action = c"sending response to client".as_ptr().cast_mut() };

    req.headers_out.status = NGX_HTTP_OK as ngx_uint_t;
    req.headers_out.content_length_n = of.size;
    req.headers_out.last_modified_time = of.mtime;

    if unsafe { ngx_http_set_etag(r) } != NGX_OK as ngx_int_t {
        return Err(SERVER_ERROR);
    }
    if unsafe { ngx_http_set_content_type(r) } != NGX_OK as ngx_int_t {
        return Err(SERVER_ERROR);
    }

    let h: *mut ngx_table_elt_t = unsafe { ngx_list_push(&mut req.headers_out.headers) }.cast();
    let h = unsafe { h.as_mut() }.ok_or(SERVER_ERROR)?;
    h.hash = 1;
    h.next = ptr::null_mut();
    h.key = ngx_string!("Content-Encoding");
    h.value = ngx_string!("zstd");
    h.lowcase_key = c"content-encoding".as_ptr().cast_mut().cast();
    req.headers_out.content_encoding = h;

    let pool = req.pool;
    let b: *mut ngx_buf_t = unsafe { ngx_pcalloc(pool, size_of::<ngx_buf_t>()) }.cast();
    let b = unsafe { b.as_mut() }.ok_or(SERVER_ERROR)?;
    let file: *mut ngx_file_t = unsafe { ngx_pcalloc(pool, size_of::<ngx_file_t>()) }.cast();
    let file = unsafe { file.as_mut() }.ok_or(SERVER_ERROR)?;

    file.fd = of.fd;
    file.name = path;
    file.log = log;
    unsafe {
        ngx_mzstd_buf_set_file(
            b,
            file,
            of.size,
            ptr::eq(r, req.main) as ngx_uint_t,
            flags.is_directio() as ngx_uint_t,
        )
    };

    let rc = unsafe { ngx_http_send_header(r) };
    if rc == NGX_ERROR as ngx_int_t
        || rc > NGX_OK as ngx_int_t
        || unsafe { ngx_mzstd_r_header_only(r) } != 0
    {
        return Ok(rc);
    }

    let mut out = ngx_chain_t {
        buf: b,
        next: ptr::null_mut(),
    };
    Ok(unsafe { ngx_http_output_filter(r, &mut out) })
}

unsafe extern "C" fn create_conf(cf: *mut ngx_conf_t) -> *mut c_void {
    let pool = unsafe { Pool::from_ngx_pool((*cf).pool) };
    pool.allocate(StaticConf { enable: UNSET_UINT }).cast()
}

unsafe extern "C" fn merge_conf(
    _cf: *mut ngx_conf_t,
    parent: *mut c_void,
    child: *mut c_void,
) -> *mut c_char {
    let prev = unsafe { &*parent.cast::<StaticConf>() };
    let conf = unsafe { &mut *child.cast::<StaticConf>() };

    if conf.enable == UNSET_UINT {
        conf.enable = if prev.enable == UNSET_UINT {
            STATIC_OFF
        } else {
            prev.enable
        };
    }

    NGX_CONF_OK
}

unsafe extern "C" fn init(cf: *mut ngx_conf_t) -> ngx_int_t {
    let cmcf = unsafe {
        (*cf).http_main_conf_unchecked::<ngx_http_core_main_conf_t>(&*ptr::addr_of!(
            ngx_http_core_module
        ))
    };
    let Some(cmcf) = cmcf.map(|p| unsafe { &mut *p.as_ptr() }) else {
        return NGX_ERROR as ngx_int_t;
    };

    let phase = &mut cmcf.phases[ngx_http_phases_NGX_HTTP_CONTENT_PHASE as usize];
    let slot: *mut ngx_http_handler_pt = unsafe { ngx_array_push(&mut phase.handlers) }.cast();
    let Some(slot) = (unsafe { slot.as_mut() }) else {
        return NGX_ERROR as ngx_int_t;
    };
    *slot = Some(handler);

    NGX_OK as ngx_int_t
}
