---
cargo/ngx_mzstd: patch
---

Update mzstd to 0.2.2. The encoder now writes straight into uninitialized output buffers, so each new `zstd_buffers` buffer is no longer zeroed first. A response whose body does not match its `Content-Length` is still aborted, now by the encoder itself. Output is unchanged.
