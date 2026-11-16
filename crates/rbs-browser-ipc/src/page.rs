//! Page boundary rules and pixel helpers every runner shares (ADR-0109 decisions 9
//! and 10). Pure functions, so the rules are tested without CEF.

/// The directory a `file` page may read: its own URL up to and including the last
/// `/`. `None` for any other scheme.
pub fn file_scope(page_url: &str) -> Option<&str> {
    if !is_file_url(page_url) {
        return None;
    }
    let end = page_url.rfind('/')?;
    // `file:///` alone has no directory below the root to bound.
    if end < "file:///".len() {
        return None;
    }
    Some(&page_url[..=end])
}

/// True when the page may load `url`. A `file` request is allowed only under the
/// page's own directory, subdirectories included (ADR-0109 decision 10); a page that
/// is not itself a `file` URL may load no `file` URL. Other schemes are the page's
/// network access, which the source already has (decision 5).
pub fn request_allowed(scope: Option<&str>, url: &str) -> bool {
    if !is_file_url(url) {
        return true;
    }
    let Some(scope) = scope else {
        return false;
    };
    let Some(rest) = url.get(scope.len()..) else {
        return false;
    };
    if !url[..scope.len()].eq_ignore_ascii_case(scope) {
        return false;
    }
    // The engine resolves `..` before this check, but an escaped dot or slash could
    // still climb out on another layer: refuse them outright.
    let lower = rest.to_ascii_lowercase();
    !(lower.contains("..")
        || lower.contains("%2e")
        || lower.contains("%2f")
        || lower.contains("%5c")
        || lower.contains('\\'))
}

fn is_file_url(url: &str) -> bool {
    url.get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file://"))
}

/// Converts premultiplied `Bgra8` (what CEF paints) into straight-alpha `Rgba8`
/// in one pass while copying into `dst`. When `transparent` is false, alpha is
/// forced opaque (decision 9). `src` and `dst` must have the same length; extra
/// bytes in the longer one are left alone.
pub fn bgra_premultiplied_to_rgba(src: &[u8], dst: &mut [u8], transparent: bool) {
    for (from, to) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let (b, g, r, a) = (from[0], from[1], from[2], from[3]);
        if !transparent {
            to.copy_from_slice(&[r, g, b, 255]);
            continue;
        }
        match a {
            0 => to.copy_from_slice(&[0, 0, 0, 0]),
            255 => to.copy_from_slice(&[r, g, b, 255]),
            _ => {
                let un = |c: u8| {
                    let value = (u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a);
                    value.min(255) as u8
                };
                to.copy_from_slice(&[un(r), un(g), un(b), a]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_page_reads_only_its_directory() {
        let scope = file_scope("file:///home/a/overlay/index.html");
        assert_eq!(scope, Some("file:///home/a/overlay/"));
        assert!(request_allowed(scope, "file:///home/a/overlay/index.html"));
        assert!(request_allowed(
            scope,
            "file:///home/a/overlay/img/logo.png"
        ));
        assert!(!request_allowed(scope, "file:///home/a/secret.txt"));
        assert!(!request_allowed(scope, "file:///home/a/overlay2/x.png"));
        assert!(!request_allowed(
            scope,
            "file:///home/a/overlay/../secret.txt"
        ));
        assert!(!request_allowed(
            scope,
            "file:///home/a/overlay/%2e%2e/secret"
        ));
        assert!(!request_allowed(scope, "file:///home/a/overlay/a%2Fb"));
        assert!(!request_allowed(scope, "file:///home/a/overlay/a\\..\\b"));
        assert!(request_allowed(scope, "https://example.com/x.js"));
    }

    #[test]
    fn a_web_page_reads_no_file() {
        let scope = file_scope("https://example.com/");
        assert_eq!(scope, None);
        assert!(!request_allowed(scope, "file:///etc/passwd"));
        assert!(!request_allowed(scope, "FILE:///etc/passwd"));
        assert!(request_allowed(scope, "data:text/plain,hi"));
    }

    #[test]
    fn the_root_is_not_a_scope() {
        assert_eq!(file_scope("file:///index.html"), None);
        assert!(!request_allowed(None, "file:///index.html"));
    }

    #[test]
    fn premultiplied_bgra_becomes_straight_rgba() {
        let src = [
            10, 20, 30, 255, // opaque
            0, 0, 0, 0, // clear
            64, 32, 0, 128, // half
        ];
        let mut dst = [0u8; 12];
        bgra_premultiplied_to_rgba(&src, &mut dst, true);
        assert_eq!(&dst[..4], &[30, 20, 10, 255]);
        assert_eq!(&dst[4..8], &[0, 0, 0, 0]);
        assert_eq!(&dst[8..], &[0, 64, 128, 128]);
        bgra_premultiplied_to_rgba(&src, &mut dst, false);
        assert!(dst.chunks_exact(4).all(|p| p[3] == 255));
        assert_eq!(&dst[..4], &[30, 20, 10, 255]);
    }
}
