---
cargo/ngx_mzstd: minor
---

`zstd_dict_file` now compresses responses with a dictionary; before, it was ignored. It accepts Zstandard-format dictionaries (such as `zstd --train` writes) and raw-content ones, as tokers/zstd-nginx-module does, and also works in `server` and `location` blocks, where `zstd_dict_file off` turns it off. The file is read and checked when the configuration is loaded, so a missing or malformed dictionary now fails the configuration instead of logging a warning. Each file is prepared once for every level and window that locations use it with. Responses with a dictionary are compressed in the event loop, also when `zstd_thread_pool` is set.

Update mzstd to 0.2.4. Responses compressed on a thread pool change: each segment now loads the input before it as history instead of compressing it again, up to 27% faster at levels 1-5 and 18-22, with frames up to 0.3% larger. Serial output is unchanged.
