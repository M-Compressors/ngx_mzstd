---
cargo/ngx_mzstd: patch
---

Update mzstd to 0.2.5. Levels -5 to 2 compress 5-10% faster, and levels 13-15 up to 21% faster. A response compressed without a dictionary allocates about 8 KB less for its encoder. Output is unchanged.
