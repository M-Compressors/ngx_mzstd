---
cargo/ngx_mzstd: patch
---

Update mzstd to 0.2.9. Reused encoders no longer slow down at levels 4-10 on responses similar to the previous ones, and levels -5 to 2 compress a few percent faster. Output is unchanged. The benchmarks in the README are measured anew.
