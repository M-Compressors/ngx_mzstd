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

## Contents

- [Example](#example)
- [Installation from packages](#installation-from-packages)
- [Building](#building)
- [Configuration directives](#configuration-directives)
  - [`zstd_static`](#zstd_static)
  - [`zstd`](#zstd)
  - [`zstd_types`](#zstd_types)
  - [`zstd_buffers`](#zstd_buffers)
  - [`zstd_comp_level`](#zstd_comp_level)
  - [`zstd_window`](#zstd_window)
  - [`zstd_min_length`](#zstd_min_length)
  - [`zstd_dict_file`](#zstd_dict_file)
  - [`zstd_reuse_memory`](#zstd_reuse_memory)
  - [`zstd_retention_policy`](#zstd_retention_policy)
  - [`zstd_idle_encoders`](#zstd_idle_encoders)
  - [`zstd_thread_pool`](#zstd_thread_pool)
  - [`zstd_parallel_segment_size`](#zstd_parallel_segment_size)
  - [`zstd_parallel_tasks`](#zstd_parallel_tasks)
  - [`zstd_parallel_compressors`](#zstd_parallel_compressors)
- [Variables](#variables)
  - [`$zstd_ratio`](#zstd_ratio)
- [Sample configuration](#sample-configuration)
- [Together with ngx_mbrotli](#together-with-ngx_mbrotli)
- [Differences from tokers/zstd-nginx-module](#differences-from-tokerszstd-nginx-module)
- [Benchmarks](#benchmarks)
  - [Latency of other requests](#latency-of-other-requests)
  - [One large response](#one-large-response)
  - [CPU time per request](#cpu-time-per-request)
  - [Thread pool overhead](#thread-pool-overhead)
- [Implementation notes](#implementation-notes)
- [Testing](#testing)
- [Contributing and releases](#contributing-and-releases)
- [License](#license)

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

### Add the repository on Debian or Ubuntu:

```sh
curl -1sLf 'https://dl.cloudsmith.io/public/m-compressors/ngx_mzstd/setup.deb.sh' | sudo -E bash
```

### On RHEL, CentOS, AlmaLinux, Rocky Linux, Amazon Linux or Fedora:

```sh
curl -1sLf 'https://dl.cloudsmith.io/public/m-compressors/ngx_mzstd/setup.rpm.sh' | sudo -E bash
```

### For nginx.org mainline, add `ngx_mzstd-mainline` instead:

```sh
curl -1sLf 'https://dl.cloudsmith.io/public/m-compressors/ngx_mzstd-mainline/setup.deb.sh' | sudo -E bash
curl -1sLf 'https://dl.cloudsmith.io/public/m-compressors/ngx_mzstd-mainline/setup.rpm.sh' | sudo -E bash
```

#### With nginx.org's nginx

```sh
sudo apt-get install nginx-module-mzstd # Debian, Ubuntu
sudo dnf install nginx-module-mzstd # RHEL-compatible, Fedora
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

#### With the distribution's nginx

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

## Benchmarks

One NGINX worker compressing static files on the fly with
tokers/zstd-nginx-module and with ngx_mzstd, both in the worker's event loop and
on a thread pool of 4 threads, with 1 or 4 tasks per response
(`zstd_parallel_tasks`). tokers/zstd-nginx-module is used as shipped: commit
`057a7d3` with libzstd 1.5.7. All run with default settings apart from
`zstd_comp_level` and, on the pool, `zstd_thread_pool` and
`zstd_parallel_tasks`, so ngx_mzstd reuses encoders and cuts responses into 1m
segments. The worker and its pool share 4 CPUs.

These are results of a reduced run under WSL2 (see *Hardware, build and
method* at the end of this section): good for comparing the modules with each
other, less so as absolute numbers.

With the thread pool:

- **Other requests are not held up.** While large responses are compressed, the
  99th percentile latency of small uncompressed requests on the same worker
  stays at 1.7–1.9 ms, as when nothing is compressed (1.9 ms). With
  tokers/zstd-nginx-module it grows to 4.0 ms at level 3 and 197 ms at level 19,
  and to about 1.5 s at level 19 when 8 large requests arrive at once.
- **One worker serves 2.2–15 times as many requests** as with
  tokers/zstd-nginx-module on responses of 85 KB and more. The gain is largest
  at levels 15–19.
- **A 2.2 MB response is sent 1.6–2.2 times as fast with 4 tasks as with 1**,
  and 1.2–1.7 times as fast as by tokers/zstd-nginx-module. Only responses
  longer than one segment gain from more tasks.
- **CPU time per request is 1.3–2.1 times higher** than in the event loop at
  levels 9–19 on responses of 1 MB and more, and higher than with
  tokers/zstd-nginx-module. See [Thread pool overhead](#thread-pool-overhead).
- **A worker needs much more memory**: each response in flight holds encoders
  for its segments, and with 8 connections a worker peaks at up to 1.1 GiB
  (level 15, 2.2 MB response, 4 tasks).

In the event loop, compared with tokers/zstd-nginx-module:

- **One worker serves 1.02–4.0 times as many requests** on responses of 85 KB
  and more, the most on jquery.min.js at level 15.
- **On 1.9 KB responses at levels 9–19, it serves 5,100–36,000 requests per
  second against 46–4,000**: tokers/zstd-nginx-module spends 0.25 ms of CPU time
  on each such response at level 9 and 18–22 ms and up to 85 MiB at levels
  15–19, likely because it never declares the response's length (see
  [Differences](#differences-from-tokerszstd-nginx-module)). With the pool,
  ngx_mzstd serves 16,000–44,000.
- **A worker's memory is no higher than tokers/zstd-nginx-module's**, give or
  take 1 MiB, and mostly lower at levels 15–19.
- **The event loop is blocked all the same.** Small requests wait 5–27% less,
  only because compression itself is faster.

### Latency of other requests

The 99th percentile latency of small uncompressed requests, 1,000 per second,
while one connection requests react-dom.development.js (1.03 MB) at a fixed rate,
about half of what tokers/zstd-nginx-module can serve, in ms:

| Level, large requests | No compression | tokers/zstd | ngx_mzstd, event loop | ngx_mzstd, pool, 1 task | ngx_mzstd, pool, 4 tasks |
|---|--:|--:|--:|--:|--:|
| 3, 178 per second | 1.9 | 4.0 | 3.8 | 1.9 | 1.7 |
| 19, 2 per second | 1.9 | 197 | 157 | 1.9 | 1.9 |

On the pool, no small request takes more than 4.4 ms. When
the large requests come from 8 connections at once, tokers/zstd-nginx-module
compresses them one after another, so at level 19 small requests wait for about
8 × 200 ms; on the pool they still stay at the uncompressed level. The large
responses themselves then take a median 322 ms with 4 tasks, 461 ms with 1 task
and 797 ms with tokers/zstd-nginx-module.

### One large response

Time to send libraries.json (2.2 MB, 3 segments of 1m) over one connection, one
response at a time, median of three runs, in ms:

| Level | tokers/zstd | ngx_mzstd, event loop | ngx_mzstd, pool, 1 task | ngx_mzstd, pool, 4 tasks |
|--:|--:|--:|--:|--:|
| 3 | 5.7 | 4.8 | 5.4 | 3.5 |
| 9 | 20.5 | 20.1 | 30.1 | 16.8 |
| 15 | 143 | 129 | 160 | 87 |
| 19 | 417 | 334 | 557 | 251 |

The gain from more tasks is bounded by the number of segments: react-dom (1 MiB
and 31 KB) is only 9–48% faster with 4 tasks than with 1, and responses up to
1 MiB do not depend on the number of tasks at all.

### CPU time per request

The worker's CPU time per request, including the pool's threads, with 8
connections, median of three runs, in ms:

| Response | Level | tokers/zstd | ngx_mzstd, event loop | ngx_mzstd, pool, 1 task | ngx_mzstd, pool, 4 tasks |
|---|--:|--:|--:|--:|--:|
| react-dom.development.js, 1.03 MB | 3 | 2.8 | 2.4 | 2.9 | 2.9 |
| react-dom.development.js, 1.03 MB | 9 | 10.9 | 10.8 | 15.6 | 16.4 |
| react-dom.development.js, 1.03 MB | 19 | 201 | 159 | 246 | 252 |
| libraries.json, 2.2 MB | 9 | 20.7 | 20.1 | 34.0 | 35.3 |
| libraries.json, 2.2 MB | 15 | 146 | 130 | 221 | 269 |
| libraries.json, 2.2 MB | 19 | 424 | 342 | 641 | 640 |

### Thread pool overhead

The extra CPU time on the pool has two measured causes:

- **Segment overlap.** Every segment but the first first runs through part of
  the input before it, so that its matches can reach back: 1/8 of the window
  at levels that use the lazy match finders (512 KiB for each 1 MiB segment of
  libraries.json), and the whole window at levels 19 and above. Without that
  overlap, compressing in segments costs no more than compressing in one piece.
  Handing segments to the pool costs almost nothing.
- **Encoders allocated anew.** A worker keeps encoders for
  `zstd_parallel_compressors` compressors, 4 by default; with 8 responses in
  flight, the others allocate their tables for every response. At level 15
  with 8 connections, `zstd_parallel_compressors 8` lowers CPU time per request
  from 218 to 186 ms with 1 task and from 279 to 193 ms with 4, and serves 17%
  and 42% more requests. The kept encoders raise the worker's peak memory from
  524 to 566 MiB with 1 task and from 1.1 to 1.6 GiB with 4.

A larger `zstd_parallel_segment_size` means fewer overlaps. With 4m, both files
here fit in one segment, so a response is compressed by one task without
overlap. With 1 task and 8 connections:

| Response | Level | CPU, event loop | CPU, 1m | CPU, 4m | Peak memory, 1m | Peak memory, 4m |
|---|--:|--:|--:|--:|--:|--:|
| react-dom.development.js, 1.03 MB | 3 | 2.4 ms | 2.9 ms | 2.4 ms | 53 MiB | 41 MiB |
| react-dom.development.js, 1.03 MB | 9 | 10.8 ms | 15.4 ms | 12.2 ms | 165 MiB | 138 MiB |
| react-dom.development.js, 1.03 MB | 15 | 64.7 ms | 86.1 ms | 75.6 ms | 393 MiB | 326 MiB |
| react-dom.development.js, 1.03 MB | 19 | 159 ms | 249 ms | 171 ms | 455 MiB | 284 MiB |
| libraries.json, 2.2 MB | 3 | 4.8 ms | 5.8 ms | 5.2 ms | 58 MiB | 60 MiB |
| libraries.json, 2.2 MB | 9 | 20.1 ms | 34.3 ms | 23.0 ms | 186 MiB | 152 MiB |
| libraries.json, 2.2 MB | 15 | 130 ms | 216 ms | 186 ms | 521 MiB | 485 MiB |
| libraries.json, 2.2 MB | 19 | 342 ms | 638 ms | 378 ms | 508 MiB | 414 MiB |

With 4m, CPU time per request is 1.02–1.17 times the event loop's instead of
1.20–1.87 times, a worker serves 15–70% more requests, and the output is the
same size as in the event loop; with 1m it is up to 1.4% larger. Level 15 on
the 2.2 MB file stays at 1.43 times, because of the encoders allocated anew.
In exchange, only responses longer than 4 MiB are compressed by more than one
thread at once.

<details>
<summary>Hardware, build and method</summary>

**Hardware**

- Intel Core i7-13700KF.
- Ubuntu 22.04.5 under WSL2 2.7.14.0, kernel 6.18.33.2-microsoft-standard-WSL2,
  on Windows 10.0.26300. WSL2 shows 12 identical cores of 2 threads each and
  does not expose which of them are performance cores.

**Build**

- NGINX 1.30.6, `./configure --with-compat --with-threads
  --without-http_rewrite_module`, gcc 11.4.0, both modules dynamic.
- tokers/zstd-nginx-module at commit `057a7d3` with libzstd 1.5.7.
- ngx_mzstd 0.2.3 with mzstd 0.2.9, `cargo build --release` (rustc 1.99.0).

**Configuration**

`worker_processes 1`, pinned to 4 CPUs on different physical cores, and
`thread_pool cp threads=4`. On the pool, `zstd_thread_pool cp` with
`zstd_parallel_tasks 1` or `4`. Everything else is left at its default,
including `zstd_parallel_segment_size 1m`, `zstd_parallel_compressors 4` and
`zstd_retention_policy aggressive`. Files are served from tmpfs.

**Load**

- Over loopback, without TLS, from 4 other physical cores, and the small
  requests of the latency test from 2 more.
- Requests per second and CPU time: wrk 4.1.0, 2 threads and 8 connections, for
  4 seconds (8 seconds for files over 1 MB, 5 times longer at level 19), after a
  1-second warm-up. Each run starts NGINX anew and waits until the worker has
  been idle for 300 ms. Three runs in a shuffled order; the tables show the
  median.
- Time to send one response: the same with 1 connection, for 4 seconds
  (10 seconds at level 19), divided by the number of responses.
- CPU time per request is the worker's user and system time over the run, from
  `/proc/<pid>/stat`, including the pool's threads; peak memory is the worker's
  `VmHWM`.
- Latency of other requests: wrk2 (`44a94c1`), open loop with coordinated
  omission corrected, two streams at once: small.json without
  compression at 1,000 requests per second over 32 connections, and the large
  compressed response at a fixed rate, over 1 connection (one 30-second run) or
  8 connections (median of three 30-second runs).
- Every response, for every module, level and file, was decoded with curl's zstd
  decoder and compared with the original file by SHA-256.

**Files**

jQuery 3.7.1 (`jquery.min.js`, 85 KB) and React DOM 18.3.1
(`react-dom.development.js`, 1.03 MB) from cdnjs, the full cdnjs API listing
(`libraries.json`, 2.2 MB), and its first 10 entries (`small.json`, 1.9 KB),
fetched on 2026-10-20. Measured on 2026-11-01.

Not measured yet: gzip, more than one worker, slow clients, long runs and
compression ratios beyond the correctness check.

</details>

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

Bug reports and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md)
for the setup and the checks a change must pass, and follow the
[Code of Conduct](CODE_OF_CONDUCT.md). If the module behaves differently from
tokers/zstd-nginx-module with the same configuration, open an issue with the
[Differs from tokers/zstd-nginx-module](https://github.com/M-Compressors/ngx_mzstd/issues/new?template=compatibility.yml)
template. Report security vulnerabilities privately as described in
[SECURITY.md](SECURITY.md), not as public issues.

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
