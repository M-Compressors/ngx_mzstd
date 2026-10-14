//! Rust side of `bitfields.c`: safe-to-call wrappers over the C accessors.
//!
//! NGINX structures carry their flags in C bit-fields, and the accessors
//! bindgen generates for them can disagree with the C compiler's layout, so
//! every flag this crate reads or writes goes through these functions.

use crate::body::BufFlags;

use ngx::ffi::{
    ngx_buf_t, ngx_file_t, ngx_http_core_loc_conf_t, ngx_http_request_t, ngx_http_variable_value_t,
    ngx_open_file_info_t, ngx_uint_t, off_t,
};

unsafe extern "C" {
    pub fn ngx_mzstd_r_header_only(r: *mut ngx_http_request_t) -> ngx_uint_t;
    pub fn ngx_mzstd_r_set_gzip_vary(r: *mut ngx_http_request_t);
    pub fn ngx_mzstd_r_claim_encoding(r: *mut ngx_http_request_t);
    pub fn ngx_mzstd_r_prepare_encoded(r: *mut ngx_http_request_t);
    pub fn ngx_mzstd_r_set_root_tested(r: *mut ngx_http_request_t);
    pub fn ngx_mzstd_r_set_buffered(r: *mut ngx_http_request_t, flag: ngx_uint_t, on: ngx_uint_t);
    pub fn ngx_mzstd_r_output_pending(r: *mut ngx_http_request_t) -> ngx_uint_t;
    #[cfg(ngx_feature = "threads")]
    pub fn ngx_mzstd_r_set_blocked(r: *mut ngx_http_request_t, on: ngx_uint_t);
    #[cfg(ngx_feature = "threads")]
    pub fn ngx_mzstd_r_thread_done(r: *mut ngx_http_request_t) -> ngx_uint_t;
    #[cfg(ngx_feature = "threads")]
    pub fn ngx_mzstd_r_thread_resume(r: *mut ngx_http_request_t);

    fn ngx_mzstd_buf_flags(b: *mut ngx_buf_t) -> ngx_uint_t;
    pub fn ngx_mzstd_buf_set_recycled(b: *mut ngx_buf_t);
    pub fn ngx_mzstd_buf_set_flush(b: *mut ngx_buf_t);
    pub fn ngx_mzstd_buf_set_last_buf(b: *mut ngx_buf_t);
    pub fn ngx_mzstd_buf_set_file(
        b: *mut ngx_buf_t,
        file: *mut ngx_file_t,
        size: off_t,
        last_buf: ngx_uint_t,
        directio: ngx_uint_t,
    );

    pub fn ngx_mzstd_var_set(v: *mut ngx_http_variable_value_t, data: *mut u8, len: usize);
    pub fn ngx_mzstd_var_set_not_found(v: *mut ngx_http_variable_value_t);

    pub fn ngx_mzstd_clcf_gzip_vary(clcf: *mut ngx_http_core_loc_conf_t) -> ngx_uint_t;

    pub fn ngx_mzstd_of_init(of: *mut ngx_open_file_info_t, clcf: *mut ngx_http_core_loc_conf_t);
    fn ngx_mzstd_of_flags(of: *mut ngx_open_file_info_t) -> ngx_uint_t;
}

/// The flags of an `ngx_buf_t`.
///
/// # Safety
///
/// `b` must point to a valid buffer.
pub unsafe fn buf_flags(b: *mut ngx_buf_t) -> BufFlags {
    let flags = unsafe { ngx_mzstd_buf_flags(b) };
    BufFlags {
        in_memory: flags & 0x01 != 0,
        in_file: flags & 0x02 != 0,
        last_buf: flags & 0x04 != 0,
        flush: flags & 0x08 != 0,
    }
}

/// Flags of an `ngx_open_file_info_t`.
#[derive(Copy, Clone)]
pub struct OpenFileFlags(ngx_uint_t);

impl OpenFileFlags {
    /// # Safety
    ///
    /// `of` must point to a valid open file info.
    pub unsafe fn of(of: *mut ngx_open_file_info_t) -> Self {
        Self(unsafe { ngx_mzstd_of_flags(of) })
    }

    pub fn is_dir(self) -> bool {
        self.0 & 0x01 != 0
    }

    pub fn is_file(self) -> bool {
        self.0 & 0x02 != 0
    }

    pub fn is_directio(self) -> bool {
        self.0 & 0x04 != 0
    }
}
