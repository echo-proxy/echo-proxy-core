use regex::Regex;
use std::path::Path;

#[derive(Debug)]
pub enum GeoSiteError {
    Io(std::io::Error),
    Decode(String),
    TagNotFound(String),
    InvalidRegex { pattern: String, source: String },
}

impl std::fmt::Display for GeoSiteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read geosite dat: {e}"),
            Self::Decode(msg) => write!(f, "failed to decode geosite dat: {msg}"),
            Self::TagNotFound(tag) => write!(f, "tag '{tag}' not found in geosite dat"),
            Self::InvalidRegex { pattern, source } => {
                write!(f, "invalid regex '{pattern}' in geosite dat: {source}")
            }
        }
    }
}

impl std::error::Error for GeoSiteError {}

impl From<std::io::Error> for GeoSiteError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// A pre-compiled set of domain matching rules extracted from a GeoSite dat.
///
/// Rule types (per domain-list-community):
/// - `Domain`  — suffix match: value `foo.com` matches `foo.com` and `bar.foo.com`.
/// - `Full`    — exact match.
/// - `Plain`   — keyword (substring) match.
/// - `Regex`   — pre-compiled regular expression.
#[derive(Debug, Default)]
pub struct GeoSiteMatcher {
    /// Suffix/root-domain rules, stored lowercased with a leading `.` added so
    /// we can do a suffix check without false positives, e.g. `notfoo.com`.
    pub domains: Vec<String>,
    /// Exact full-domain rules, stored lowercased.
    pub fulls: Vec<String>,
    /// Keyword (substring) rules, stored lowercased.
    pub keywords: Vec<String>,
    /// Pre-compiled regular expressions.
    pub regexes: Vec<Regex>,
}

impl GeoSiteMatcher {
    /// Returns `true` if `host` (already lowercase, without trailing `.`) is
    /// matched by any rule in this matcher.
    pub fn matches(&self, host: &str) -> bool {
        // Full match.
        if self.fulls.iter().any(|f| f == host) {
            return true;
        }
        // Keyword match.
        if self.keywords.iter().any(|k| host.contains(k.as_str())) {
            return true;
        }
        // Suffix / root-domain match.
        // A rule ".foo.com" matches "foo.com" and "bar.foo.com" but not "notfoo.com".
        if self.domains.iter().any(|d| {
            host == &d[1..] // exact apex: host == "foo.com" and d == ".foo.com"
                || host.ends_with(d.as_str()) // subdomain: host ends with ".foo.com"
        }) {
            return true;
        }
        // Regex match.
        if self.regexes.iter().any(|r| r.is_match(host)) {
            return true;
        }
        false
    }
}

/// Load a V2Ray GeoSite dat file and return a combined [`GeoSiteMatcher`] for
/// all requested tags.
///
/// Tags are compared case-insensitively against the `country_code` field.
/// Returns an error if the file is missing, cannot be decoded, any tag is
/// absent, or any regex rule fails to compile.
pub fn load_geosite_dat(path: &Path, tags: &[String]) -> Result<GeoSiteMatcher, GeoSiteError> {
    let bytes = std::fs::read(path)?;
    let list =
        geosite_rs::decode_geosite(&bytes).map_err(|e| GeoSiteError::Decode(e.to_string()))?;

    let mut matcher = GeoSiteMatcher::default();

    for tag in tags {
        let tag_upper = tag.to_uppercase();
        let entry = list
            .entry
            .iter()
            .find(|e| e.country_code.to_uppercase() == tag_upper)
            .ok_or_else(|| GeoSiteError::TagNotFound(tag.clone()))?;

        for domain in &entry.domain {
            let value = domain.value.to_lowercase();
            match domain.r#type {
                // Domain = 2 — root/suffix match
                2 => matcher.domains.push(format!(".{value}")),
                // Full = 3 — exact match
                3 => matcher.fulls.push(value),
                // Plain = 0 — keyword match
                0 => matcher.keywords.push(value),
                // Regex = 1 — compile and store
                1 => {
                    let re = Regex::new(&value).map_err(|e| GeoSiteError::InvalidRegex {
                        pattern: value.clone(),
                        source: e.to_string(),
                    })?;
                    matcher.regexes.push(re);
                }
                _ => {}
            }
        }
    }

    Ok(matcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(
        domains: &[&str],
        fulls: &[&str],
        keywords: &[&str],
        regexes: &[&str],
    ) -> GeoSiteMatcher {
        GeoSiteMatcher {
            domains: domains.iter().map(|s| format!(".{s}")).collect(),
            fulls: fulls.iter().map(|s| s.to_string()).collect(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            regexes: regexes.iter().map(|s| Regex::new(s).unwrap()).collect(),
        }
    }

    #[test]
    fn domain_suffix_matches_apex_and_subdomain() {
        let m = matcher(&["foo.com"], &[], &[], &[]);
        assert!(m.matches("foo.com"));
        assert!(m.matches("bar.foo.com"));
        assert!(m.matches("deep.bar.foo.com"));
        assert!(!m.matches("notfoo.com"));
        assert!(!m.matches("xfoo.com"));
    }

    #[test]
    fn full_domain_matches_exactly() {
        let m = matcher(&[], &["www.foo.com"], &[], &[]);
        assert!(m.matches("www.foo.com"));
        assert!(!m.matches("foo.com"));
        assert!(!m.matches("bar.foo.com"));
    }

    #[test]
    fn keyword_matches_substring() {
        let m = matcher(&[], &[], &["baidu"], &[]);
        assert!(m.matches("www.baidu.com"));
        assert!(m.matches("baidu.com"));
        assert!(m.matches("tieba.baidu.com"));
        assert!(!m.matches("notba-idu.com"));
    }

    #[test]
    fn regex_matches_pattern() {
        let m = matcher(&[], &[], &[], &[r"^.*\.cn$"]);
        assert!(m.matches("example.cn"));
        assert!(m.matches("sub.example.cn"));
        assert!(!m.matches("example.com"));
    }

    #[test]
    fn missing_file_returns_io_error() {
        let err = load_geosite_dat(Path::new("nonexistent.dat"), &["cn".to_string()]).unwrap_err();
        assert!(matches!(err, GeoSiteError::Io(_)));
    }
}
