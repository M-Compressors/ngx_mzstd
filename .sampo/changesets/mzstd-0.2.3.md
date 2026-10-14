---
cargo/ngx_mzstd: patch
---

Update mzstd to 0.2.3. Compression is faster at every level: the fast and double-fast match finders (levels up to 4), the lazy finders and the optimal parser (levels 16–22) do less work per byte, and sequence coding executes fewer instructions. Output is unchanged and still byte-identical to zstd 1.5.7.
