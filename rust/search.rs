use crate::{
    error::{invalid, Result},
    target::Target,
    transport::Transport,
};
use std::collections::HashSet;
use url::Url;

fn parts(url: &Url) -> Result<(&str, &str)> {
    let path = url.path().strip_prefix("/en").unwrap_or(url.path());
    let mut parts = path.trim_end_matches('/').split('/');
    if parts.next() != Some("") || parts.next() != Some("tags") {
        return Err(invalid("Use a Pixiv tag-search URL"));
    }
    let tag = parts
        .next()
        .filter(|s| !s.is_empty() && !s.contains(".."))
        .ok_or_else(|| invalid("Missing or invalid search tag"))?;
    let category = parts.next().unwrap_or("all");
    if !["all", "artworks", "illustrations", "manga", "novels"].contains(&category)
        || parts.next().is_some()
    {
        return Err(invalid("Unsupported tag-search category"));
    }
    Ok((tag, category))
}

pub fn validate(url: &Url) -> Result<()> {
    parts(url)?;
    let mut seen = HashSet::new();
    for (key, value) in url.query_pairs() {
        if !seen.insert(key.clone()) {
            return Err(invalid("Duplicate tag-search parameter"));
        }
        if ![
            "word",
            "order",
            "mode",
            "p",
            "r",
            "s_mode",
            "type",
            "lang",
            "gs",
            "csw",
            "work_lang",
            "ai_type",
            "scd",
            "ecd",
            "wlt",
            "wgt",
            "hlt",
            "hgt",
            "ratio",
            "tool",
            "blt",
            "bgt",
        ]
        .contains(&key.as_ref())
            || value.chars().any(char::is_control)
        {
            return Err(invalid(&format!("Unsupported tag-search parameter: {key}")));
        }
        let valid = match key.as_ref() {
            "mode" => matches!(value.as_ref(), "safe" | "all"),
            "order" => matches!(value.as_ref(), "date_d" | "date"),
            "s_mode" => matches!(
                value.as_ref(),
                "s_tag_full" | "s_tag" | "s_tag_only" | "s_tc"
            ),
            "gs" => value == "0",
            "p" => value.parse::<u32>().is_ok_and(|n| n > 0),
            "r" => matches!(value.as_ref(), "0" | "1"),
            _ => true,
        };
        if !valid {
            return Err(invalid(&format!("Unsupported tag-search value: {key}")));
        }
    }
    Ok(())
}

pub fn discover(client: &dyn Transport, source: &str, types: &str) -> Result<Vec<Target>> {
    let url = Url::parse(source).map_err(|_| invalid("Invalid search URL"))?;
    validate(&url)?;
    let (tag, category) = parts(&url)?;
    // Preserve path characters that would otherwise change form-query parsing.
    let encoded = format!("word={}", tag.replace('+', "%2B").replace('&', "%26"));
    let word = url::form_urlencoded::parse(encoded.as_bytes())
        .next()
        .unwrap()
        .1
        .into_owned();
    if word.chars().any(char::is_control) {
        return Err(invalid("Invalid search tag"));
    }
    let mut refs = Vec::new();
    for (endpoint, node, kind) in [
        ("artworks", "illustManga", "art"),
        ("illustrations", "illust", "art"),
        ("manga", "manga", "art"),
        ("novels", "novel", "novel"),
    ] {
        if (category != endpoint
            && !(category == "all" && matches!(endpoint, "artworks" | "novels")))
            || (types != "all" && types != kind)
        {
            continue;
        }
        let mut query: Vec<(&str, String)> = vec![
            ("word", word.clone()),
            ("order", "date_d".into()),
            ("mode", "safe".into()),
            ("s_mode", "s_tag_full".into()),
        ];
        let pairs: Vec<_> = url.query_pairs().collect();
        for (key, value) in &pairs {
            if key == "p" {
                continue;
            }
            if let Some(pair) = query.iter_mut().find(|(k, _)| *k == key.as_ref()) {
                pair.1 = value.to_string();
            } else {
                query.push((key.as_ref(), value.to_string()));
            }
        }
        query.push(("p", "1".into()));
        let mut seen = HashSet::new();
        let mut expected = 0;
        for page in 1..=100_000 {
            query.last_mut().unwrap().1 = page.to_string();
            let body = client.api(&format!("/ajax/search/{endpoint}/{tag}"), &query)?;
            let result = &body[node];
            let entries = result["data"]
                .as_array()
                .ok_or_else(|| invalid("Tag-search result schema changed"))?;
            let total = result["total"]
                .as_u64()
                .ok_or_else(|| invalid("Missing tag-search total"))?;
            expected = expected.max(total);
            if expected > 100_000 {
                return Err(invalid("Search exceeds 100000 works; narrow the search"));
            }
            let before = seen.len();
            for item in entries {
                if item["isAdContainer"] == true {
                    continue;
                }
                let id = item["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| item["id"].to_string());
                let target = Target::new(kind, &id)?;
                if seen.insert(target.clone()) {
                    refs.push(target);
                }
            }
            if refs.len() > 100_000 {
                return Err(invalid("Search exceeds 100000 works; narrow the search"));
            }
            if seen.len() as u64 > expected {
                return Err(invalid("Tag-search total changed; run the search again"));
            }
            if seen.len() as u64 >= expected {
                break;
            }
            if seen.len() == before || result["lastPage"].as_u64().is_some_and(|last| page >= last)
            {
                return Err(invalid(&format!(
                    "Search incomplete: found {} of {expected} {kind} works; results changed or the search range is limited. Narrow the search and try again",
                    seen.len()
                )));
            }
        }
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::Mutex;

    type Call = (String, Vec<(String, String)>);
    #[derive(Default)]
    struct Search {
        pages: Vec<Value>,
        calls: Mutex<Vec<Call>>,
    }
    impl Transport for Search {
        fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
            let mut calls = self.calls.lock().unwrap();
            let page = self
                .pages
                .get(calls.len())
                .cloned()
                .ok_or_else(|| invalid("Unexpected search request"))?;
            calls.push((
                path.into(),
                query
                    .iter()
                    .map(|(k, v)| ((*k).into(), v.clone()))
                    .collect(),
            ));
            Ok(page)
        }
        fn download(&self, _: &str, _: &std::path::Path) -> Result<()> {
            unreachable!()
        }
        fn requests(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn stopped(&self) -> bool {
            false
        }
        fn cancel(&self) {}
    }

    #[test]
    fn all_2600_results_are_paginated_without_a_500_work_limit() {
        let pages = (1..=2600).collect::<Vec<_>>().chunks(60).map(|ids| {
            json!({"illustManga":{"data":ids.iter().map(|id| json!({"id":id.to_string()})).collect::<Vec<_>>(),"total":2600,"lastPage":44}})
        }).collect();
        let client = Search {
            pages,
            ..Default::default()
        };
        let results = discover(
            &client,
            "https://www.pixiv.net/tags/test/artworks?p=9",
            "all",
        )
        .unwrap();
        assert_eq!(results.len(), 2600);
        assert_eq!(results.last().unwrap().label(), "art:2600");
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 44);
        assert_eq!(calls[0].1.last().unwrap(), &("p".into(), "1".into()));
        assert_eq!(calls[43].1.last().unwrap(), &("p".into(), "44".into()));
    }

    #[test]
    fn duplicates_categories_and_url_filters_are_preserved() {
        let client = Search {
            pages: vec![
                json!({"illustManga":{"data":[{"id":"1"},{"id":2},{"isAdContainer":true}],"total":3}}),
                json!({"illustManga":{"data":[{"id":"2"},{"id":"3"}],"total":3}}),
                json!({"novel":{"data":[{"id":"4"}],"total":1}}),
            ],
            ..Default::default()
        };
        let results = discover(&client, "https://www.pixiv.net/en/tags/C++?word=%E7%8C%AB+%E7%8A%AC&mode=all&s_mode=s_tag&order=date", "all").unwrap();
        assert_eq!(
            results.iter().map(Target::label).collect::<Vec<_>>(),
            ["art:1", "art:2", "art:3", "novel:4"]
        );
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls[0].0, "/ajax/search/artworks/C++");
        assert_eq!(calls[2].0, "/ajax/search/novels/C++");
        for pair in [
            ("word", "猫 犬"),
            ("mode", "all"),
            ("s_mode", "s_tag"),
            ("order", "date"),
        ] {
            assert!(calls[0].1.contains(&(pair.0.into(), pair.1.into())));
        }
        drop(calls);
        for word in ["C++", "A&B", "A=B"] {
            let client = Search {
                pages: vec![json!({"novel":{"data":[],"total":0}})],
                ..Default::default()
            };
            discover(
                &client,
                &format!("https://www.pixiv.net/tags/{word}/novels"),
                "novel",
            )
            .unwrap();
            assert!(client.calls.lock().unwrap()[0]
                .1
                .contains(&("word".into(), word.into())));
        }
    }

    #[test]
    fn novel_search_keeps_browser_parameters_without_requesting_artworks() {
        let client = Search {
            pages: vec![json!({"novel":{"data":[{"id":"1004"}],"total":1}})],
            ..Default::default()
        };
        let refs = discover(
            &client,
            "https://www.pixiv.net/tags/%E7%8D%A3%E4%BA%BA/novels?p=2&work_lang=ja&r=1",
            "all",
        )
        .unwrap();
        assert_eq!(refs[0].label(), "novel:1004");
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "/ajax/search/novels/%E7%8D%A3%E4%BA%BA");
        for (key, value) in [("work_lang", "ja"), ("r", "1"), ("p", "1")] {
            assert!(calls[0].1.contains(&(key.into(), value.into())));
        }
    }
    #[test]
    fn each_category_uses_its_own_schema_and_type_filter() {
        for (category, node, kind) in [
            ("artworks", "illustManga", "art"),
            ("illustrations", "illust", "art"),
            ("manga", "manga", "art"),
            ("novels", "novel", "novel"),
        ] {
            let client = Search {
                pages: vec![json!({node:{"data":[{"id":"1"}],"total":1}})],
                ..Default::default()
            };
            let url = format!("https://www.pixiv.net/tags/%E7%8C%AB/{category}");
            assert_eq!(discover(&client, &url, "all").unwrap()[0].kind, kind);
            assert!(client.calls.lock().unwrap()[0]
                .1
                .contains(&("word".into(), "猫".into())));
            assert!(discover(
                &Search::default(),
                &url,
                if kind == "art" { "novel" } else { "art" }
            )
            .unwrap()
            .is_empty());
        }
    }

    #[test]
    fn incomplete_or_incompatible_results_never_report_success() {
        for pages in [
            vec![json!({"illustManga":{"data":[],"total":10}})],
            vec![json!({"illustManga":{"data":[{"id":"1"}],"total":10,"lastPage":1}})],
            vec![
                json!({"illustManga":{"data":[{"id":"1"}],"total":10}}),
                json!({"illustManga":{"data":[{"id":"1"}],"total":10}}),
            ],
            vec![json!({"illustManga":{"data":[{"id":"bad"}],"total":1}})],
            vec![json!({"illustManga":{"data":[{"id":"1"}],"total":0}})],
            vec![json!({"illustManga":{"data":[],"total":100001}})],
            vec![json!({"illustManga":{"data":[]}})],
            vec![json!({"unexpected":[]})],
        ] {
            assert!(discover(
                &Search {
                    pages,
                    ..Default::default()
                },
                "https://www.pixiv.net/tags/test/artworks",
                "all"
            )
            .is_err());
        }
    }
}
