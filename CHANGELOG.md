# ngx_mzstd

## 0.2.4 — 2026-11-01

### Patch changes

- [aac0519](https://github.com/M-Compressors/ngx_mzstd/commit/aac0519ec52ad211ab034853d7f9dafb9223f41e) Update mzstd to 0.2.9. Reused encoders no longer slow down at levels 4-10 on responses similar to the previous ones, and levels -5 to 2 compress a few percent faster. Output is unchanged. The benchmarks in the README are measured anew. — Thanks @Mnwa!

## 0.2.3 — 2026-10-20

### Patch changes

- [f524020](https://github.com/M-Compressors/ngx_mzstd/commit/f524020cf8901c85f711c3cee4fd114a4be2026c) Prebuilt deb and rpm packages for amd64 and arm64 on Cloudsmith: `nginx-module-mzstd` for nginx.org's stable and mainline nginx, on every distribution release nginx.org packages for, and `libnginx-mod-http-mzstd` and `nginx-mod-mzstd` for the nginx of Debian, Ubuntu, RHEL-compatible distributions and Fedora, from 1.25.4 on. See "Installation from packages" in the README. — Thanks @Mnwa!

## 0.2.2 — 2026-10-18

### Patch changes

- Bump zstd with perf optimizations

## 0.2.1 — 2026-10-15

### Patch changes

- [03f5719](https://github.com/M-Compressors/ngx_mzstd/commit/03f57198664464238b24405447120027b531c752) Update mzstd to 0.2.5. Levels -5 to 2 compress 5-10% faster, and levels 13-15 up to 21% faster. A response compressed without a dictionary allocates about 8 KB less for its encoder. Output is unchanged. — Thanks @Mnwa!

## 0.2.0 — 2026-10-15

### Minor changes

- [4834385](https://github.com/M-Compressors/ngx_mzstd/commit/4834385dc27db26e51445bbbcd0709c374f961c4) `zstd_dict_file` now compresses responses with a dictionary; before, it was ignored. It accepts Zstandard-format dictionaries (such as `zstd --train` writes) and raw-content ones, as tokers/zstd-nginx-module does, and also works in `server` and `location` blocks, where `zstd_dict_file off` turns it off. The file is read and checked when the configuration is loaded, so a missing or malformed dictionary now fails the configuration instead of logging a warning. Each file is prepared once for every level and window that locations use it with. Responses with a dictionary are compressed in the event loop, also when `zstd_thread_pool` is set.
  
  Update mzstd to 0.2.4. Responses compressed on a thread pool change: each segment now loads the input before it as history instead of compressing it again, up to 27% faster at levels 1-5 and 18-22, with frames up to 0.3% larger. Serial output is unchanged. — Thanks @Mnwa!

## 0.1.2 — 2026-10-14

### Patch changes

- [146f881](https://github.com/M-Compressors/ngx_mzstd/commit/146f881762b57a65d5621837d84135d0048dd007) Update mzstd to 0.2.3. Compression is faster at every level: the fast and double-fast match finders (levels up to 4), the lazy finders and the optimal parser (levels 16–22) do less work per byte, and sequence coding executes fewer instructions. Output is unchanged and still byte-identical to zstd 1.5.7. — Thanks @Mnwa!

## 0.1.1 — 2026-10-14

### Patch changes

- [935b72a](https://github.com/M-Compressors/ngx_mzstd/commit/935b72a808090cb61971c84a6346f3e46e4e7638) Update mzstd to 0.2.2. The encoder now writes straight into uninitialized output buffers, so each new `zstd_buffers` buffer is no longer zeroed first. A response whose body does not match its `Content-Length` is still aborted, now by the encoder itself. Output is unchanged. — Thanks @Mnwa!

## 0.1.0 — 2026-10-14

### Minor changes

- [b4da15a](https://github.com/M-Compressors/ngx_mzstd/commit/b4da15a717938cd50e3e82ee298f1ed767fda6ff) Initial release: `ngx_http_zstd_filter_module` and `ngx_http_zstd_static_module`, a drop-in replacement for tokers/zstd-nginx-module built on ngx-rust and the pure-Rust mzstd encoder. Supports its directives (`zstd`, `zstd_types`, `zstd_buffers`, `zstd_comp_level` including negative levels, `zstd_min_length`, `zstd_static`; `zstd_dict_file` is accepted and ignored) and the `$zstd_ratio` variable, and adds those of ngx_mbrotli: `zstd_window`, `zstd_reuse_memory`, `zstd_retention_policy`, `zstd_idle_encoders`, `zstd_thread_pool`, `zstd_parallel_segment_size`, `zstd_parallel_tasks` and `zstd_parallel_compressors`. The window is limited to 8 MB, as RFC 9659 requires and browsers enforce, so levels 20–22 no longer produce responses Chrome rejects. With ngx_mbrotli loaded too, zstd is preferred when a client accepts both encodings. — Thanks @Mnwa for your first contribution 🎉!

