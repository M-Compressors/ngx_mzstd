---
cargo/ngx_mzstd: patch
---

Fix the build against freenginx, where `ngx_buf_tag_t` is `uintptr_t` rather than `void *`. CI now runs the unit and integration tests against freenginx too.
