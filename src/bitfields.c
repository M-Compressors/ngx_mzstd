/*
 * Bit-field accessors for NGINX structures.
 *
 * bindgen does not always reproduce the C compiler's bit-field layout: in
 * nginx 1.30 the bit-fields of ngx_http_request_t follow a 16-bit `port`
 * member, and the accessors nginx-sys generates are off by two bits from
 * `uri_changes` onwards. Every bit-field this crate touches is therefore read
 * and written here, compiled against the same headers as NGINX itself.
 */

#include <ngx_config.h>
#include <ngx_core.h>
#include <ngx_http.h>


/* ngx_http_request_t */

ngx_uint_t
ngx_mzstd_r_header_only(ngx_http_request_t *r)
{
    return r->header_only;
}


void
ngx_mzstd_r_set_gzip_vary(ngx_http_request_t *r)
{
#if (NGX_HTTP_GZIP)
    r->gzip_vary = 1;
#endif
}


/* The response is (or will be) zstd-encoded: keep the gzip filter out. */
void
ngx_mzstd_r_claim_encoding(ngx_http_request_t *r)
{
#if (NGX_HTTP_GZIP)
    r->gzip_tested = 1;
    r->gzip_ok = 0;
#endif
}


/* Prepares the headers of a response the body filter is going to encode. */
void
ngx_mzstd_r_prepare_encoded(ngx_http_request_t *r)
{
    r->main_filter_need_in_memory = 1;

    ngx_http_clear_content_length(r);
    ngx_http_clear_accept_ranges(r);
    ngx_http_weak_etag(r);
}


void
ngx_mzstd_r_set_root_tested(ngx_http_request_t *r)
{
    r->root_tested = !r->error_page;
}


void
ngx_mzstd_r_set_buffered(ngx_http_request_t *r, ngx_uint_t flag,
    ngx_uint_t on)
{
    if (on) {
        r->connection->buffered |= flag;

    } else {
        r->connection->buffered &= ~flag;
    }
}


/* Whether the write filter has anything left to send for `r`. */
ngx_uint_t
ngx_mzstd_r_output_pending(ngx_http_request_t *r)
{
    return r->out != NULL
           || (r->connection->buffered & NGX_LOWLEVEL_BUFFERED);
}


#if (NGX_THREADS)

/* Keeps the request from being freed while thread tasks refer to it. */
void
ngx_mzstd_r_set_blocked(ngx_http_request_t *r, ngx_uint_t on)
{
    if (on) {
        r->main->blocked++;

    } else {
        r->main->blocked--;
    }
}


/*
 * Called when a thread task of `r` completes. Returns whether the request
 * has been finalized or terminated meanwhile, and so is only waiting for its
 * tasks to let go of it.
 */
ngx_uint_t
ngx_mzstd_r_thread_done(ngx_http_request_t *r)
{
    ngx_http_set_log_request(r->connection->log, r);

    return r->done || r->main->terminated;
}


/*
 * Resumes the connection of `r` after a thread task, as a write event would:
 * its active request may be a subrequest whose output waits for the filter,
 * or a finalized or terminated request waiting for the task to let go.
 */
void
ngx_mzstd_r_thread_resume(ngx_http_request_t *r)
{
    ngx_connection_t  *c;

    c = r->connection;
    c->write->handler(c->write);
}

#endif


/* ngx_buf_t */

#define NGX_MZSTD_BUF_IN_MEMORY  0x01
#define NGX_MZSTD_BUF_IN_FILE    0x02
#define NGX_MZSTD_BUF_LAST_BUF   0x04
#define NGX_MZSTD_BUF_FLUSH      0x08


ngx_uint_t
ngx_mzstd_buf_flags(ngx_buf_t *b)
{
    ngx_uint_t  flags;

    flags = 0;

    if (ngx_buf_in_memory(b)) {
        flags |= NGX_MZSTD_BUF_IN_MEMORY;
    }

    if (b->in_file) {
        flags |= NGX_MZSTD_BUF_IN_FILE;
    }

    if (b->last_buf) {
        flags |= NGX_MZSTD_BUF_LAST_BUF;
    }

    if (b->flush) {
        flags |= NGX_MZSTD_BUF_FLUSH;
    }

    return flags;
}


void
ngx_mzstd_buf_set_recycled(ngx_buf_t *b)
{
    b->recycled = 1;
}


void
ngx_mzstd_buf_set_flush(ngx_buf_t *b)
{
    b->flush = 1;
}


/* Marks the final buffer; an empty one must not claim to hold data. */
void
ngx_mzstd_buf_set_last_buf(ngx_buf_t *b)
{
    if (b->pos == b->last) {
        b->temporary = 0;
    }

    b->last_buf = 1;
}


/* Points `b` at the whole of `file`. */
void
ngx_mzstd_buf_set_file(ngx_buf_t *b, ngx_file_t *file, off_t size,
    ngx_uint_t last_buf, ngx_uint_t directio)
{
    b->file_pos = 0;
    b->file_last = size;
    b->in_file = size ? 1 : 0;
    b->last_buf = last_buf ? 1 : 0;
    b->last_in_chain = 1;
    b->file = file;

    file->directio = directio ? 1 : 0;
}


/*
 * ngx_http_variable_value_t
 *
 * $zstd_ratio changes while the response is compressed, so its value is never
 * cached, as in tokers/zstd-nginx-module.
 */

void
ngx_mzstd_var_set(ngx_http_variable_value_t *v, u_char *data, size_t len)
{
    v->data = data;
    v->len = len;
    v->valid = 1;
    v->no_cacheable = 1;
    v->not_found = 0;
}


void
ngx_mzstd_var_set_not_found(ngx_http_variable_value_t *v)
{
    v->valid = 1;
    v->no_cacheable = 1;
    v->not_found = 1;
}


/* ngx_http_core_loc_conf_t */

/* Whether `gzip_vary` is on in the location `clcf` configures. */
ngx_uint_t
ngx_mzstd_clcf_gzip_vary(ngx_http_core_loc_conf_t *clcf)
{
#if (NGX_HTTP_GZIP)
    return clcf->gzip_vary ? 1 : 0;
#else
    return 0;
#endif
}


/* ngx_open_file_info_t */

#define NGX_MZSTD_OF_IS_DIR       0x01
#define NGX_MZSTD_OF_IS_FILE      0x02
#define NGX_MZSTD_OF_IS_DIRECTIO  0x04


void
ngx_mzstd_of_init(ngx_open_file_info_t *of, ngx_http_core_loc_conf_t *clcf)
{
    ngx_memzero(of, sizeof(ngx_open_file_info_t));

    of->read_ahead = clcf->read_ahead;
    of->directio = clcf->directio;
    of->valid = clcf->open_file_cache_valid;
    of->min_uses = clcf->open_file_cache_min_uses;
    of->errors = clcf->open_file_cache_errors;
    of->events = clcf->open_file_cache_events;
}


ngx_uint_t
ngx_mzstd_of_flags(ngx_open_file_info_t *of)
{
    ngx_uint_t  flags;

    flags = 0;

    if (of->is_dir) {
        flags |= NGX_MZSTD_OF_IS_DIR;
    }

    if (of->is_file) {
        flags |= NGX_MZSTD_OF_IS_FILE;
    }

    if (of->is_directio) {
        flags |= NGX_MZSTD_OF_IS_DIRECTIO;
    }

    return flags;
}
