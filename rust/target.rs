use crate::error::{invalid, Result};
use serde::{Deserialize, Serialize};
use url::Url;

pub const MAX_WORKS: usize = 10_000_000;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Target {
    pub kind: String,
    pub id: String,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 20
        && !id.starts_with('0')
        && id.bytes().all(|b| b.is_ascii_digit())
}

impl Target {
    pub fn new(kind: &str, id: &str) -> Result<Self> {
        if !["art", "novel", "user", "series"].contains(&kind) || !valid_id(id) {
            return Err(invalid("Use a positive ID of at most 20 digits"));
        }
        Ok(Self {
            kind: kind.into(),
            id: id.into(),
        })
    }
    pub fn parse(input: &str) -> Result<Self> {
        if let Some((kind, id)) = input.split_once(':') {
            if ["art", "novel", "user", "series"].contains(&kind) {
                return Self::new(kind, id);
            }
        }
        let u = Url::parse(input).map_err(|_| invalid("Invalid Pixiv URL"))?;
        if u.scheme() != "https"
            || !matches!(u.host_str(), Some("pixiv.net" | "www.pixiv.net"))
            || !u.username().is_empty()
            || u.password().is_some()
            || u.port_or_known_default() != Some(443)
            || input.chars().any(char::is_control)
            || input.contains('\\')
        {
            return Err(invalid("Use a canonical HTTPS Pixiv URL"));
        }
        if matches!(u.path().trim_end_matches('/'), "/novel" | "/en/novel") {
            if u.query().is_some() || u.fragment().is_some() {
                return Err(invalid(
                    "Use the novel top URL without filters for an ID scan",
                ));
            }
            return Ok(Self {
                kind: "novel_scan".into(),
                id: "https://www.pixiv.net/novel".into(),
            });
        }
        if u.path().starts_with("/tags/")
            || u.path().starts_with("/en/tags/")
            || matches!(u.path().trim_end_matches('/'), "/search" | "/en/search")
        {
            crate::search::validate(&u)?;
            let mut u = u;
            u.set_fragment(None);
            return Ok(Self {
                kind: "search".into(),
                id: u.to_string(),
            });
        }
        let path = u
            .path()
            .strip_prefix("/en/")
            .unwrap_or(u.path().trim_start_matches('/'))
            .trim_end_matches('/');
        for (prefix, kind) in [
            ("users/", "user"),
            ("artworks/", "art"),
            ("novel/series/", "series"),
        ] {
            if let Some(id) = path.strip_prefix(prefix) {
                return Self::new(kind, id);
            }
        }
        if path == "novel/show.php" {
            let ids: Vec<_> = u.query_pairs().filter(|(k, _)| k == "id").collect();
            if ids.len() == 1 {
                return Self::new("novel", &ids[0].1);
            }
        }
        Err(invalid("Use a work, author, novel-series or search URL"))
    }
    pub fn key(&self) -> String {
        format!("{}_{}", self.kind, self.id)
    }
    pub fn label(&self) -> String {
        if matches!(self.kind.as_str(), "search" | "novel_scan") {
            return self.id.clone();
        }
        format!("{}:{}", self.kind, self.id)
    }
    pub fn url(&self) -> String {
        if matches!(self.kind.as_str(), "search" | "novel_scan") {
            return self.id.clone();
        }
        if self.kind == "novel" {
            format!("https://www.pixiv.net/novel/show.php?id={}", self.id)
        } else {
            format!("https://www.pixiv.net/artworks/{}", self.id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn novel_top_is_a_pinned_scan_source_without_browser_filters() {
        for url in [
            "https://www.pixiv.net/novel",
            "https://pixiv.net/novel/",
            "https://www.pixiv.net/en/novel",
        ] {
            let target = Target::parse(url).unwrap();
            assert_eq!(target.kind, "novel_scan");
            assert_eq!(target.url(), "https://www.pixiv.net/novel");
            assert_eq!(Target::parse(&target.label()).unwrap(), target);
        }
        assert!(Target::parse("https://www.pixiv.net/novel?mode=all").is_err());
    }
    #[test]
    fn canonical_and_fragment() {
        assert_eq!(
            Target::parse("https://www.pixiv.net/artworks/130117333#1")
                .unwrap()
                .label(),
            "art:130117333"
        );
        assert_eq!(
            Target::parse("https://www.pixiv.net/en/novel/show.php?id=29476682")
                .unwrap()
                .label(),
            "novel:29476682"
        );
        assert_eq!(
            Target::parse("https://pixiv.net/novel/series/8001")
                .unwrap()
                .kind,
            "series"
        );
    }
    #[test]
    fn reject_bad_origins_and_ids() {
        for v in [
            "art:0",
            "novel:01",
            "user:-1",
            "art:123456789012345678901",
            "https://www.pixiv.net.evil/artworks/1",
            "https://u:p@www.pixiv.net/artworks/1",
            "http://www.pixiv.net/artworks/1",
            "https://www.pixiv.net/novel/show.php?id=1&id=2",
        ] {
            assert!(Target::parse(v).is_err(), "{v}");
        }
    }
    #[test]
    fn search_urls_round_trip_and_reject_unsupported_filters() {
        for url in [
            "https://www.pixiv.net/tags/%E7%8C%AB/artworks?p=2&s_mode=s_tag_full",
            "https://www.pixiv.net/en/tags/C++/novels",
            "https://www.pixiv.net/tags/%E7%8D%A3%E4%BA%BA/novels?p=2&work_lang=ja&r=1",
            "https://www.pixiv.net/tags/test/novels?r=0",
            "https://pixiv.net/tags/test",
            "https://www.pixiv.net/search?q=R18&s_mode=tag_tc&type=novel",
            "https://www.pixiv.net/en/search/?q=%E7%8C%AB+%E7%8A%AC&type=novel&work_lang=ja&r=1",
        ] {
            let target = Target::parse(url).unwrap();
            assert_eq!(target.kind, "search");
            assert_eq!(Target::parse(&target.label()).unwrap(), target);
            assert_eq!(target.url(), target.label());
        }
        for url in [
            "https://www.pixiv.net/tags//artworks",
            "https://www.pixiv.net/tags/test/users",
            "https://www.pixiv.net/tags/test/novels?gs=1",
            "https://www.pixiv.net/tags/test/artworks?mode=r18",
            "https://www.pixiv.net/tags/test/artworks?order=popular_d",
            "https://www.pixiv.net/tags/test/artworks?unknown=1",
            "https://www.pixiv.net/tags/test/novels?r=2",
            "https://www.pixiv.net/tags/test/novels?r=1&r=0",
            "https://www.pixiv.net/tags/test/artworks?p=0",
            "https://www.pixiv.net/tags/test/artworks?mode=all&mode=safe",
            "https://www.pixiv.net.evil/tags/test/artworks",
            "https://www.pixiv.net/search?q=R18&q=other&type=novel",
            "https://www.pixiv.net/search?q=R18&type=novel&type=novel",
            "https://www.pixiv.net/search?q=&type=novel",
            "https://www.pixiv.net/search?q=%0A&type=novel",
            "https://www.pixiv.net/search?q=.&type=novel",
            "https://www.pixiv.net/search?q=R18&word=other&type=novel",
            "https://www.pixiv.net/search?q=R18&type=unknown",
            "https://www.pixiv.net/search?q=R18",
            "https://www.pixiv.net/search?type=novel",
            "https://www.pixiv.net/search?q=R18&type=novel&s_mode=unknown",
            "https://www.pixiv.net/search?q=R18&type=novel&s_mode=tag_tc&s_mode=tag",
            "https://www.pixiv.net/search?q=R18&type=novel&unknown=1",
        ] {
            assert!(Target::parse(url).is_err(), "{url}");
        }
    }
}
