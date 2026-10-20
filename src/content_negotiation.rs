//! Content negotiation based on the HTTP `Accept` header.

struct MediaRange<'a> {
    type_: &'a str,
    subtype: &'a str,
    q: f32,
}

impl<'a> MediaRange<'a> {
    fn parse(entry: &'a str) -> Option<Self> {
        let mut parts = entry.split(';').map(str::trim);
        let (type_, subtype) = match parts.next()? {
            "" => return None,
            "*" => ("*", "*"),
            mime => mime.split_once('/')?,
        };
        let mut q = 1.0;
        for param in parts {
            if let Some((key, value)) = param.split_once('=') {
                if key.trim().eq_ignore_ascii_case("q") {
                    // Out-of-range or malformed quality values count as 1.
                    q = value
                        .trim()
                        .parse()
                        .ok()
                        .filter(|q| (0.0..=1.0).contains(q))
                        .unwrap_or(1.0);
                }
            }
        }
        Some(Self {
            type_: type_.trim(),
            subtype: subtype.trim(),
            q,
        })
    }

    /// How specifically this range matches `type_/subtype`, or None if it
    /// doesn't match at all.
    fn specificity(&self, type_: &str, subtype: &str) -> Option<u8> {
        let type_exact = self.type_.eq_ignore_ascii_case(type_);
        let subtype_exact = self.subtype.eq_ignore_ascii_case(subtype);
        if !(type_exact || self.type_ == "*") || !(subtype_exact || self.subtype == "*") {
            return None;
        }
        Some(2 * type_exact as u8 + subtype_exact as u8)
    }
}

/// Pick the entry of `supported` that best matches an `Accept` header.
///
/// `supported` is in order of server preference, most preferred first.
/// Each supported type gets the quality of the most specific range in the
/// header that matches it. The highest quality wins; ties go to the type
/// matched by the more specific range, then to the earlier entry in
/// `supported`. A missing header is treated as `*/*`.
///
/// Returns None if no supported type is acceptable, which callers
/// should turn into 406 Not Acceptable.
pub fn best_match<'a>(supported: &[&'a str], accept: Option<&str>) -> Option<&'a str> {
    let ranges: Vec<MediaRange> = accept
        .unwrap_or("*/*")
        .split(',')
        .filter_map(|entry| MediaRange::parse(entry.trim()))
        .collect();

    let mut best: Option<(f32, u8, &'a str)> = None;
    for &candidate in supported {
        let Some((type_, subtype)) = candidate.split_once('/') else {
            continue;
        };
        let Some((q, specificity)) = ranges
            .iter()
            .filter_map(|r| Some((r.q, r.specificity(type_, subtype)?)))
            .fold(None, |acc: Option<(f32, u8)>, (q, s)| match acc {
                Some((_, best_s)) if best_s >= s => acc,
                _ => Some((q, s)),
            })
        else {
            continue;
        };
        if q <= 0.0 {
            continue;
        }
        let better = match best {
            None => true,
            Some((best_q, best_s, _)) => q > best_q || (q == best_q && specificity > best_s),
        };
        if better {
            best = Some((q, specificity, candidate));
        }
    }
    best.map(|(_, _, candidate)| candidate)
}

#[cfg(test)]
mod tests {
    use super::best_match;

    const SUPPORTED: &[&str] = &["text/html", "application/json", "text/plain"];

    #[test]
    fn test_best_match() {
        let cases: &[(Option<&str>, Option<&str>)] = &[
            (None, Some("text/html")),
            (Some("*/*"), Some("text/html")),
            (Some("*"), Some("text/html")),
            (Some("text/*"), Some("text/html")),
            (Some("application/json"), Some("application/json")),
            (Some("text/plain"), Some("text/plain")),
            (Some("TEXT/Plain"), Some("text/plain")),
            (
                Some("text/plain, application/json"),
                Some("application/json"),
            ),
            (Some("text/html, application/json"), Some("text/html")),
            (
                Some("application/json, */*;q=0.1"),
                Some("application/json"),
            ),
            (
                Some("application/json, text/html;q=0.1"),
                Some("application/json"),
            ),
            (Some("text/plain;q=0.5, */*"), Some("text/html")),
            // The more specific range wins, even if a wildcard has a higher q.
            (Some("text/html;q=0.2, */*"), Some("application/json")),
            (Some("text/*, application/json"), Some("application/json")),
            (Some("text/html;q=0, text/*"), Some("text/plain")),
            (Some("*/*;q=0"), None),
            (
                Some("text/plain;q=bogus, text/html;q=0.5"),
                Some("text/plain"),
            ),
            (Some("text/plain;q=2, text/html;q=0.5"), Some("text/plain")),
            (Some("image/png"), None),
            (Some(""), None),
            (Some(" , ,"), None),
        ];
        for (accept, expected) in cases {
            assert_eq!(
                best_match(SUPPORTED, *accept),
                *expected,
                "Accept: {:?}",
                accept
            );
        }
    }
}
