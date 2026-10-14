//! `Accept-Encoding` negotiation shared by the filter and static modules.

#[cfg(not(test))]
use ngx::ffi::ngx_http_request_t;

#[cfg(not(test))]
use crate::bitfields::ngx_mzstd_r_claim_encoding;

const TOKEN: &[u8] = b"zstd";

/// Returns `true` when the client accepts the `zstd` content coding.
///
/// As in tokers/zstd-nginx-module, the first `zstd` token decides; unlike it,
/// the token is rejected when its quality value is zero (`q=0`, `q=0.`,
/// `q=0.0`, ... `q=0.000`), as google/ngx_brotli does for `br`.
pub fn accepts_zstd(header: &[u8]) -> bool {
    let mut from = 0;

    while let Some(offset) = find_token(&header[from..]) {
        let start = from + offset;
        let end = start + TOKEN.len();
        from = end;

        let before = if start == 0 { b' ' } else { header[start - 1] };
        let after = header.get(end).copied().unwrap_or(b' ');

        if before != b',' && !is_space(before) {
            continue;
        }
        if after != b',' && after != b';' && !is_space(after) {
            continue;
        }

        return !has_zero_quality(&header[end..]);
    }

    false
}

/// Checks the eligibility of `r` for a `zstd` response and, when it is eligible,
/// tells the gzip modules to stay out of the way.
///
/// # Safety
///
/// `r` must point to a valid request.
#[cfg(not(test))]
pub unsafe fn check_request(r: *mut ngx_http_request_t) -> bool {
    let r = unsafe { &mut *r };

    if !core::ptr::eq(r, r.main) {
        return false;
    }

    let Some(entry) = (unsafe { r.headers_in.accept_encoding.as_ref() }) else {
        return false;
    };

    if !accepts_zstd(entry.value.as_bytes()) {
        return false;
    }

    unsafe { ngx_mzstd_r_claim_encoding(r) };
    true
}

fn find_token(haystack: &[u8]) -> Option<usize> {
    haystack
        .windows(TOKEN.len())
        .position(|w| w.eq_ignore_ascii_case(TOKEN))
}

fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// Parses an optional `;q=<value>` parameter and reports whether it is zero.
fn has_zero_quality(rest: &[u8]) -> bool {
    let mut it = rest.iter().copied().peekable();
    let skip_spaces = |it: &mut core::iter::Peekable<_>| {
        while it.next_if(|&c| is_space(c)).is_some() {}
    };

    skip_spaces(&mut it);
    if it.next() != Some(b';') {
        return false;
    }
    skip_spaces(&mut it);
    if !matches!(it.next(), Some(b'q' | b'Q')) {
        return false;
    }
    skip_spaces(&mut it);
    if it.next() != Some(b'=') {
        return false;
    }
    skip_spaces(&mut it);
    if it.next() != Some(b'0') {
        return false;
    }
    if it.next() != Some(b'.') {
        return true;
    }

    for _ in 0..3 {
        match it.next() {
            Some(b'0') => {}
            Some(b'1'..=b'9') => return false,
            _ => return true,
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::accepts_zstd;

    #[test]
    fn negotiation() {
        let accepted: &[&[u8]] = &[
            b"zstd",
            b"ZSTD",
            b"gzip, deflate, br, zstd",
            b"zstd, gzip",
            b"gzip,zstd",
            b"gzip, zstd;q=1",
            b"gzip, zstd ; q = 0.5",
            b"zstd;q=0.001",
            b"zstd;q=1.0",
            b"zstd;level=5",
            b"zstdx, zstd",
            b"br,\tzstd",
        ];
        for header in accepted {
            assert!(accepts_zstd(header), "{}", String::from_utf8_lossy(header));
        }

        let rejected: &[&[u8]] = &[
            b"",
            b"gzip",
            b"zst",
            b"zstdx",
            b"xzstd",
            b"gzip, zstd;q=0",
            b"zstd;q=0.",
            b"zstd;q=0.0",
            b"zstd;q=0.00",
            b"zstd;q=0.000",
            b"zstd; q=0, gzip",
            b"zstd;q=0.0000",
        ];
        for header in rejected {
            assert!(!accepts_zstd(header), "{}", String::from_utf8_lossy(header));
        }
    }
}
