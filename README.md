# ngx_mzstd

Zstandard compression for NGINX, written in Rust with
[ngx-rust](https://github.com/nginx/ngx-rust) and the
[mzstd](https://github.com/M-Compressors/mzstd) encoder.

It is a drop-in replacement for
[tokers/zstd-nginx-module](https://github.com/tokers/zstd-nginx-module): the same
two modules, directives, defaults and variable, so existing configurations keep
working unchanged.

- `ngx_http_zstd_filter_module` compresses responses on the fly.
- `ngx_http_zstd_static_module` serves pre-compressed `.zst` files.

Both modules are built into one dynamic library, so one `load_module` line loads
both. It has the same extensions as its sibling
[ngx_mbrotli](https://github.com/M-Compressors/ngx_mbrotli): encoders are reused
between responses, and one line, `zstd_thread_pool default;`, takes compression
out of the worker's event loop and spreads it over the threads of an NGINX
thread pool.

## Example

Compression on a thread pool, so that long responses and high levels do not hold
up the worker's event loop. NGINX must be built with `--with-threads`:

```nginx
load_module /path/to/libngx_mzstd.so;

# The pool named `default` exists without being declared: 32 threads per worker
# process, shared with `aio threads`. To size compression on its own, declare
# a pool of its own and name it in `zstd_thread_pool`:
#
#     thread_pool zstd threads=4;

http {
    zstd on;
    zstd_comp_level 3;
    zstd_types text/plain text/css application/javascript application/json
               image/svg+xml;
    zstd_static on;

    # Responses are cut into 1m segments; up to 4 of them per response are
    # compressed at once on the pool's threads.
    zstd_thread_pool default;
    zstd_parallel_segment_size 1m;
    zstd_parallel_tasks 4;

    server {
        listen 80;

        location / {
            root /var/www;
        }

        # Small API responses: compressing them in the event loop is cheaper.
        location /api/ {
            proxy_pass http://127.0.0.1:8080;
            zstd_thread_pool off;
        }
    }
}
```

Without `zstd_thread_pool` the module compresses in the event loop, as
tokers/zstd-nginx-module does. See [Configuration directives](#configuration-directives)
for every directive.

## Installation from packages

Prebuilt packages for amd64 and arm64 are published to
[Cloudsmith](https://cloudsmith.io/~m-compressors/repos/), each built and tested
against the exact nginx package it is for. Add the repository, then install the
package that matches your nginx:

| Your nginx | Repository | Package | Enabled on install |
|---|---|---|---|
| nginx.org stable | `ngx_mzstd` | `nginx-module-mzstd` | no |
| nginx.org mainline | `ngx_mzstd-mainline` | `nginx-module-mzstd` | no |
| Debian's or Ubuntu's own | `ngx_mzstd` | `libnginx-mod-http-mzstd` | yes |
| RHEL-compatible or Fedora's own | `ngx_mzstd` | `nginx-mod-mzstd` | yes |

Packages for nginx.org's nginx are built for every distribution release that
nginx.org currently packages (Debian, Ubuntu, RHEL 8 to 10 and compatible,
Amazon Linux 2023). Packages for the distributions' own nginx are built for
Debian, Ubuntu, RHEL-compatible (the default AppStream nginx) and the current
Fedora releases, where that nginx is 1.25.4 or newer, the oldest version the
module builds against: today Debian 13, Ubuntu 26.04, RHEL 10 and Fedora.

Add the repository on Debian or Ubuntu:

```sh
curl -1sLf 'https://dl.cloudsmith.io/public/m-compressors/ngx_mzstd/setup.deb.sh' | sudo -E bash
```

On RHEL, AlmaLinux, Rocky Linux, Amazon Linux or Fedora:

```sh
curl -1sLf 'https://dl.cloudsmith.io/public/m-compressors/ngx_mzstd/setup.rpm.sh' | sudo -E bash
```

For nginx.org mainline, add `ngx_mzstd-mainline` instead:

```sh
curl -1sLf 'https://dl.cloudsmith.io/public/m-compressors/ngx_mzstd-mainline/setup.deb.sh' | sudo -E bash
curl -1sLf 'https://dl.cloudsmith.io/public/m-compressors/ngx_mzstd-mainline/setup.rpm.sh' | sudo -E bash
```

### With nginx.org's nginx

```sh
sudo apt-get install nginx-module-mzstd   # or: sudo dnf install nginx-module-mzstd
```

Like nginx.org's own `nginx-module-*` packages, it installs the module without
enabling it. Add this line to the top of `/etc/nginx/nginx.conf` and reload
nginx:

```nginx
load_module modules/ngx_mzstd.so;
```

The package requires the exact nginx package it was built for, so upgrade both
together. Its version is `<nginx version>+<module version>`, as with nginx.org's
module packages.

### With the distribution's nginx

```sh
sudo apt-get install libnginx-mod-http-mzstd   # Debian, Ubuntu
sudo dnf install nginx-mod-mzstd               # RHEL-compatible, Fedora
```

As with the distribution's own module packages, the module is enabled on
installation: by `/etc/nginx/modules-enabled/50-mod-http-mzstd.conf` on Debian
and Ubuntu, and `/usr/share/nginx/modules/mod-mzstd.conf` on RHEL and Fedora.
Configure `zstd` (see [Configuration directives](#configuration-directives)) and
reload nginx.

The packages conflict with those of tokers/zstd-nginx-module
(`nginx-module-zstd`, and the distribution's own where it has one), which
provide the same directives. They are built and published by the
[Publish workflow](.github/workflows/publish.yml).

Package repository hosting is graciously provided by [Cloudsmith](https://cloudsmith.com).

## Building

Requirements:

- Rust 1.89 or newer;
- libclang (used by `nginx-sys` to generate bindings);
- the NGINX source tree, configured with the same arguments as the NGINX binary
  that will load the module, plus `--with-compat`. Run `nginx -V` to see those
  arguments. Only `./configure` is required, not `make`.

```sh
cd nginx-1.30.5
./configure --with-compat <arguments from nginx -V>

cd /path/to/ngx_mzstd
NGINX_BUILD_DIR=/path/to/nginx-1.30.5/objs cargo build --release
```

The module is written to `target/release/libngx_mzstd.so`
(`libngx_mzstd.dylib` on macOS). Load it at the top level of `nginx.conf`:

```nginx
load_module /path/to/libngx_mzstd.so;
```

The zstd C library is not needed. To build against a vendored NGINX instead,
enable the `vendored` feature (see [nginx-src](https://docs.rs/nginx-src/)).
Prefer building against the exact sources and configuration that run in
production.

## Configuration directives

### `zstd_static`

- **syntax**: `zstd_static on|off|always`
- **default**: `off`
- **context**: `http`, `server`, `location`

Enables or disables checking for pre-compressed files with the `.zst` extension.
With `always`, the pre-compressed file is served whether or not the client
supports it. With `on` and
[`gzip_vary`](https://nginx.org/en/docs/http/ngx_http_gzip_module.html#gzip_vary),
`Vary: Accept-Encoding` is added whenever the `.zst` file exists, as with
`gzip_static`.

### `zstd`

- **syntax**: `zstd on|off`
- **default**: `off`
- **context**: `http`, `server`, `location`, `if`

Enables or disables on-the-fly compression of responses.

### `zstd_types`

- **syntax**: `zstd_types <mime_type> [..]`
- **default**: `text/html`
- **context**: `http`, `server`, `location`

Compresses responses with the listed MIME types as well as `text/html`, which is
always compressed. The special value `*` matches any MIME type.

### `zstd_buffers`

- **syntax**: `zstd_buffers <number> <size>`
- **default**: `32 4k|16 8k`
- **context**: `http`, `server`, `location`

Sets the number and size of the buffers that hold compressed output. The default
is one memory page per buffer, up to 128k in total. It works the same way as
[`gzip_buffers`](https://nginx.org/en/docs/http/ngx_http_gzip_module.html#gzip_buffers).

### `zstd_comp_level`

- **syntax**: `zstd_comp_level <level>`
- **default**: `1`
- **context**: `http`, `server`, `location`

Sets the compression level, from `1` to `22`, or a negative level from `-1` to
`-131072` for faster compression at a lower ratio. Level `0` is not accepted.

Levels 20–22 would need a window larger than 8 MB, which browsers reject; they
are limited by `zstd_window`.

### `zstd_window`

- **syntax**: `zstd_window <size>`
- **default**: `8m`
- **context**: `http`, `server`, `location`

Sets the largest window, the distance back that matches may reach. Acceptable
values are `1k`, `2k`, `4k`, `8k`, `16k`, `32k`, `64k`, `128k`, `256k`, `512k`,
`1m`, `2m`, `4m` and `8m`. A level whose own window is no larger is not
affected, and when the response length is known, a smaller window is used if it
is enough.

[RFC 9659](https://www.rfc-editor.org/rfc/rfc9659) limits the window of the
`zstd` content coding to 8 MB, and Chrome rejects responses that need more. With
the default, levels 1–19 compress exactly as without a limit, and levels 20–22
stay within 8 MB.

*Not in tokers/zstd-nginx-module.*

### `zstd_min_length`

- **syntax**: `zstd_min_length <length>`
- **default**: `20`
- **context**: `http`, `server`, `location`

Sets the minimum length of a response that will be compressed. The length comes
only from the `Content-Length` response header.

### `zstd_dict_file`

- **syntax**: `zstd_dict_file /path/to/dict | off`
- **default**: `off`
- **context**: `http`, `server`, `location`

Compresses responses with the dictionary in the file. A relative path is
taken relative to the directory of `nginx.conf`. Both kinds of dictionary
that zstd reads are accepted: one in the Zstandard format, such as
`zstd --train` produces, whose ID each frame names, and raw content, any file
without the dictionary magic number, which the frames do not name.

**Only clients that already have the dictionary can decode these responses.**
They are sent with `Content-Encoding: zstd`, as with tokers/zstd-nginx-module,
and nothing in the response tells a client which dictionary it needs; browsers
do not have it and fail to decode them. Use it only for clients you control.
Compression dictionary transport ([RFC 9842](https://www.rfc-editor.org/rfc/rfc9842),
`Content-Encoding: dcz`) is not supported.

The file is read once, when the configuration is loaded, and a malformed
Zstandard dictionary is rejected there. It is prepared once for each
`zstd_comp_level` and `zstd_window` that locations with `zstd on` use it with,
and the prepared copies are shared by all such locations. With `off`, a
location inherits no dictionary.

A dictionary helps most with short responses that resemble it, such as small
JSON documents; on long ones the gain is small. Responses compressed with a
dictionary are compressed in the event loop even with `zstd_thread_pool`:
the parallel compressor does not take dictionaries.

*In tokers/zstd-nginx-module it is only allowed in `http` and has no `off`.*

### `zstd_reuse_memory`

- **syntax**: `zstd_reuse_memory on|off`
- **default**: `on`
- **context**: `http`, `server`, `location`

Enables or disables reusing encoders between responses. When on, each worker
keeps up to `zstd_idle_encoders` idle encoders, so a response can reuse the
match finder tables, window and buffers a previous one allocated. How much
memory an idle encoder keeps is set by `zstd_retention_policy`. When off, every
response gets a new encoder that is freed when the response is complete, as
with tokers/zstd-nginx-module.

*Not in tokers/zstd-nginx-module.*

### `zstd_retention_policy`

- **syntax**: `zstd_retention_policy aggressive | current_config | release_all | bounded <size>`
- **default**: `aggressive`
- **context**: `http`, `server`, `location`

Sets how much working memory an encoder keeps between responses. It matters only
when `zstd_reuse_memory` is on.

- `aggressive` keeps every buffer, so a worker allocates only what its largest
  responses grow into. This is the fastest option, but each worker keeps memory
  for its largest responses at its highest `zstd_comp_level`.
- `current_config` keeps buffers only while they suit the current level and
  window. They are released when the encoder is reused for a location with
  different settings.
- `bounded <size>` keeps buffers while they fit within `size`, and releases them
  after a response that grew past it.
- `release_all` keeps nothing between responses; only the encoder itself is reused.

Idle encoders are reused only by locations with the same retention policy; when a
worker's `zstd_idle_encoders` slots are full, the least recently used encoder is
freed.

With `zstd_thread_pool`, an idle compressor keeps the encoders its segments
used, up to `zstd_parallel_tasks` of them, and the policy applies to all of them
together. A response holds its compressor until it has been sent, including
while its segments wait for a free thread, so a worker keeps encoders for at
most `zstd_parallel_compressors` compressors, idle or in use; other responses
allocate encoders for their segments and free them once they have been sent.
An idle compressor is reused only for the same level, window and segment size.

*Not in tokers/zstd-nginx-module.*

### `zstd_idle_encoders`

- **syntax**: `zstd_idle_encoders <number>`
- **default**: `4`
- **context**: `http`

Sets how many idle encoders each worker keeps for reuse with
`zstd_reuse_memory` on, from `0` to `1024`; when they are all taken, the least
recently used one is freed. Compressors for `zstd_thread_pool` are kept
separately, up to the same number. With `0` nothing is kept, so every response
gets a new encoder.

Idle encoders are reused only by locations with the same
`zstd_retention_policy`, so a worker that serves locations with several
policies may need more of them.

*Not in tokers/zstd-nginx-module.*

### `zstd_thread_pool`

- **syntax**: `zstd_thread_pool <name> | off`
- **default**: `off`
- **context**: `http`, `server`, `location`

Compresses responses on the named thread pool instead of in the worker's event
loop, so that a long response or a high `zstd_comp_level` does not hold up the
worker's other connections. The pool is declared with
[`thread_pool`](https://nginx.org/en/docs/ngx_core_module.html#thread_pool) in the
main context; a pool named `default` with 32 threads exists without being
declared. Requires NGINX built with `--with-threads`.

A response is cut into segments of `zstd_parallel_segment_size`, and up to
`zstd_parallel_tasks` of them are compressed on the pool's threads at once. They
are sent in order as they complete, while the next segments are read. A response
that fits in one segment is compressed in one piece by one thread. Each segment
is compressed with only a limited overlap of the input before it, so a response
longer than one segment compresses slightly worse, and a flush (for example with
`proxy_buffering off`) ends the current segment early. The response is still
one standard zstd frame.

Every worker process has its own threads, so `worker_processes` times the pool's
`threads` run at once. A response being compressed holds up to
`zstd_parallel_tasks` + 1 segments of input, their compressed output, and an
encoder for each segment being compressed.

*Not in tokers/zstd-nginx-module.*

### `zstd_parallel_segment_size`

- **syntax**: `zstd_parallel_segment_size <size>`
- **default**: `1m`
- **context**: `http`, `server`, `location`

Sets the size of the segments a response is cut into for `zstd_thread_pool`,
from `64k` to `16m`. The larger the segments, the better a response compresses,
but the fewer of its segments are compressed at once. Smaller segments spread
responses over more threads and compress worse.

*Not in tokers/zstd-nginx-module.*

### `zstd_parallel_tasks`

- **syntax**: `zstd_parallel_tasks <number>`
- **default**: `4`
- **context**: `http`, `server`, `location`

Sets how many segments of one response may be compressed, or wait to be sent, at
once with `zstd_thread_pool`, from `1` to `1024`. When they all are, NGINX stops
reading the response until the first of them has been sent.

*Not in tokers/zstd-nginx-module.*

### `zstd_parallel_compressors`

- **syntax**: `zstd_parallel_compressors <number>`
- **default**: `4`
- **context**: `http`

Sets for how many compressors used with `zstd_thread_pool` each worker keeps
segment encoders, from `0` to `1024`, counting both idle ones and ones held by
responses. A response holds its compressor until it has been sent, including
while its segments wait for a free thread, so without a limit every response in
flight could keep the encoders a previous response grew. Responses beyond the
limit allocate encoders for their segments and free them once they have been
sent. Idle compressors are also limited by `zstd_idle_encoders`. Requires NGINX
built with `--with-threads`.

*Not in tokers/zstd-nginx-module.*

## Variables

### `$zstd_ratio`

The achieved compression ratio: the original size divided by the compressed
size, truncated to three decimal places (for example `3.254`), as in
tokers/zstd-nginx-module.

## Sample configuration

```nginx
load_module /path/to/libngx_mzstd.so;

http {
    zstd on;
    zstd_comp_level 3;
    zstd_static on;
    zstd_types application/atom+xml application/javascript application/json application/vnd.api+json application/rss+xml
               application/vnd.ms-fontobject application/x-font-opentype application/x-font-truetype
               application/x-font-ttf application/x-javascript application/xhtml+xml application/xml
               font/eot font/opentype font/otf font/truetype image/svg+xml image/vnd.microsoft.icon
               image/x-icon image/x-win-bitmap text/css text/javascript text/plain text/xml;
}
```

As with tokers/zstd-nginx-module, the `Vary: Accept-Encoding` header is added
only when [`gzip_vary`](https://nginx.org/en/docs/http/ngx_http_gzip_module.html#gzip_vary)
is on.

## Together with ngx_mbrotli

Both libraries can be loaded at once, in either order. When a client accepts
both encodings, zstd is chosen, for `.zst` files over `.br` ones and for
compression on the fly; clients that accept only Brotli or gzip get those.

## Differences from tokers/zstd-nginx-module

- **The window is limited to 8 MB** by default (`zstd_window`). The C module
  never declares a response's length, so levels 20–22 declare windows of 32,
  64 and 128 MB on every response, however short, and browsers reject them.
  Here they stay within 8 MB, where long responses compress exactly as at
  level 19, as they do with the C library given the same window.
- **The length comes from `Content-Length`.** When it is known, it is declared to
  the encoder, which writes it into the frame header and chooses its parameters
  for that size, so short responses use smaller windows and tables. The C module
  never declares it, so its output differs on such responses. A response whose
  body does not match its declared length is aborted.
- **Accept-Encoding is parsed more strictly.** `zstd;q=0` is a refusal, and only
  a whole `zstd` token counts: the C module accepts any header that starts with
  `zstd`, such as `zstdx`.
- **`zstd_comp_level -1` works.** The C module takes -1 for "unset" and
  compresses at level 1 instead.
- **`zstd_dict_file` can be set per server and location**, and turned off
  with `off`. A Zstandard-format dictionary is checked when the configuration
  is loaded.
- **Encoders are reused** by default (`zstd_reuse_memory`), each keeping all
  of its working memory between responses (`zstd_retention_policy`). Set
  `zstd_reuse_memory off` to allocate per response as the C module does.
- **Responses can be compressed on a thread pool** (`zstd_thread_pool`), in
  segments that are compressed in parallel.
- The pure-Rust mzstd encoder replaces the C library. Driven with the same
  calls and parameters, it writes the same bytes as zstd 1.5.7; output compressed
  on a thread pool is a valid frame but differs.

## Implementation notes

The filter sits in the same place in the filter chain as tokers/zstd-nginx-module's,
just above the gzip filter. The dynamic library exports an `ngx_module_order` table
so that NGINX inserts it there.

All access to C bit-fields (request, buffer, variable, location and open-file
flags) goes through `src/bitfields.c`, compiled against the same headers as
NGINX. The bit-field accessors that bindgen generates for `ngx_http_request_t`
do not match the C compiler's layout in NGINX 1.30 (they are two bits off from
`uri_changes` onwards), so they cannot be used safely.

With `zstd_thread_pool`, each segment becomes an `ngx_thread_task_t`. Its
completion handler runs in the event loop and calls the zstd filter to send
what is ready, then runs the connection's write handler, as a write event would.
While tasks are posted, the request is blocked (`r->main->blocked`), so that
terminating it waits for them, and stays buffered, so that it is not finalized
before the last completion handler has run, even when the response has already
been sent.

## Testing

Unit tests do not need a running NGINX:

```sh
NGINX_BUILD_DIR=/path/to/nginx/objs cargo test
```

The integration tests in `t/` use the
[nginx-tests](https://github.com/nginx/nginx-tests) framework and need the
`zstd` command-line tool:

```sh
git clone https://github.com/nginx/nginx-tests
TEST_NGINX_BINARY=/path/to/nginx/objs/nginx \
TEST_NGINX_GLOBALS="load_module $PWD/target/release/libngx_mzstd.so;" \
    prove -I nginx-tests/lib t/
```

They decode large responses with `zstd -M8MB`, which rejects a window over 8 MB
as browsers do.

## Contributing and releases

CI (`.github/workflows/ci.yml`) runs on pull requests and pushes to `main`:

- rustfmt and clippy;
- unit and integration tests against NGINX 1.28, 1.30 and 1.31, built with
  `--with-threads`; clippy also checks a build without it;
- the integration tests under AddressSanitizer and LeakSanitizer, with NGINX
  and the module (including the Rust standard library) instrumented, and
  `NGX_DEBUG_PALLOC` so that each pool allocation is checked on its own;
- the unit tests under Miri. The body filter (`src/body.rs`) reaches NGINX only
  through a small `Host` trait, so Miri runs it against a model of NGINX's
  buffer and chain API (`src/body/tests.rs`). The model frees drained input
  buffers and chain links, and leaves output buffers uninitialized, so stale
  pointers and reads of unwritten memory show up as undefined behaviour. Its
  thread pool is either a queue that the tests run and report on their own
  schedule, or real threads, so that Miri also looks for data races between
  the filter and the segments being encoded.

Changelogs and versions are managed with [Sampo](https://github.com/bruits/sampo).
Every user-facing change needs a changeset, a short Markdown file in
`.sampo/changesets/`:

```sh
cargo install sampo
sampo add          # pick the bump level (patch, minor, major) and describe the change
```

On every push to `main`, the release workflow (`.github/workflows/release.yml`)
collects the pending changesets into a **Release (main)** pull request that bumps
the version in `Cargo.toml` and updates `CHANGELOG.md`. Merging that pull request
tags the release (`vX.Y.Z`) and publishes a GitHub Release with the changelog
entry. The crate is not published to crates.io.

For the workflow to open pull requests, enable *Settings → Actions → General →
Allow GitHub Actions to create and approve pull requests*.

Packages are published by hand: run the **Publish** workflow
(`.github/workflows/publish.yml`) on the release tag. It builds a package for
every target, tests each in a clean container of its distribution (install,
`nginx -t`, a zstd-compressed response with and without a thread pool,
removal), and pushes them to Cloudsmith only if all of them pass. With
`dry_run` it builds and tests without pushing, also from a branch. A rerun on
the same tag skips the packages already published. Targets come from nginx.org
and the distributions at run time (`.github/scripts/publish-matrix.py`, which
also says how to add a distribution); `nginx_stable` and `nginx_mainline`
override the nginx.org versions.

## License

MIT; see [LICENSE](LICENSE). The directives and their behaviour follow
[tokers/zstd-nginx-module](https://github.com/tokers/zstd-nginx-module) (BSD 2-Clause).
