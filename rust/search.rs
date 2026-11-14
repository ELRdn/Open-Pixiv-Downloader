use crate::{
    error::{invalid, Error, Result},
    target::{Target, MAX_WORKS},
    transport::Transport,
};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate};
use serde_json::Value;
use std::collections::HashSet;
use url::Url;

enum RangeTask {
    Search(Option<NaiveDate>, Option<NaiveDate>),
    Verify {
        before: usize,
        expected: u64,
        observed: Vec<Target>,
    },
}

fn normalize(url: &Url) -> Result<Url> {
    let path = url.path().strip_prefix("/en").unwrap_or(url.path());
    if path.trim_end_matches('/') != "/search" {
        return Ok(url.clone());
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let parameter = |name| {
        let values: Vec<_> = pairs.iter().filter(|(key, _)| key == name).collect();
        if values.len() != 1 || values[0].1.trim().is_empty() {
            return Err(invalid(&format!(
                "Supply exactly one search parameter: {name}"
            )));
        }
        Ok(values[0].1.as_ref())
    };
    let word = parameter("q")?;
    if word.chars().any(char::is_control) || word == "." || word.contains("..") {
        return Err(invalid("Invalid search query"));
    }
    if pairs.iter().any(|(key, _)| key == "word") {
        return Err(invalid("Use q alone for the /search query"));
    }
    if parameter("type")? != "novel" {
        return Err(invalid("/search currently requires type=novel"));
    }
    let mut normalized = url.clone();
    normalized.set_path("/tags/");
    normalized
        .path_segments_mut()
        .map_err(|_| invalid("Invalid search URL"))?
        .pop_if_empty()
        .push(word)
        .push("novels");
    normalized.set_query(None);
    {
        let mut query = normalized.query_pairs_mut();
        for (key, value) in &pairs {
            if matches!(key.as_ref(), "q" | "type") {
                continue;
            }
            let value = if key == "s_mode" {
                match value.as_ref() {
                    "tag_tc" => "s_tag",
                    "tag" => "s_tag_only",
                    "tag_full" => "s_tag_full",
                    "text" => "s_tc",
                    other => other,
                }
            } else {
                value.as_ref()
            };
            query.append_pair(key, value);
        }
        if !pairs.iter().any(|(key, _)| key == "s_mode") {
            query.append_pair("s_mode", "s_tag_full");
        }
    }
    Ok(normalized)
}

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
    let url = normalize(url)?;
    parts(&url)?;
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
            "scd" | "ecd" => NaiveDate::parse_from_str(&value, "%Y-%m-%d").is_ok(),
            _ => true,
        };
        if !valid {
            return Err(invalid(&format!("Unsupported tag-search value: {key}")));
        }
    }
    let start = url
        .query_pairs()
        .find(|(key, _)| key == "scd")
        .map(|(_, value)| value.into_owned());
    let end = url
        .query_pairs()
        .find(|(key, _)| key == "ecd")
        .map(|(_, value)| value.into_owned());
    if start
        .as_ref()
        .zip(end.as_ref())
        .is_some_and(|(start, end)| start > end)
    {
        return Err(invalid("Search start date must not follow its end date"));
    }
    Ok(())
}

pub fn discover(client: &dyn Transport, source: &str, types: &str) -> Result<Vec<Target>> {
    discover_with(
        client,
        source,
        types,
        |path, query, _| client.api(path, query),
        |_| {},
    )
}

pub fn discover_with(
    client: &dyn Transport,
    source: &str,
    types: &str,
    mut fetch: impl FnMut(&str, &[(&str, String)], &str) -> Result<Value>,
    mut progress: impl FnMut(String),
) -> Result<Vec<Target>> {
    let url = normalize(&Url::parse(source).map_err(|_| invalid("Invalid search URL"))?)?;
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
        let mut base: Vec<(&str, String)> = vec![
            ("word", word.clone()),
            ("order", "date_d".into()),
            ("mode", "safe".into()),
            ("s_mode", "s_tag_full".into()),
        ];
        let pairs: Vec<_> = url.query_pairs().collect();
        for (key, value) in &pairs {
            if matches!(key.as_ref(), "p" | "scd" | "ecd") {
                continue;
            }
            if let Some(pair) = base.iter_mut().find(|(k, _)| *k == key.as_ref()) {
                pair.1 = value.to_string();
            } else {
                base.push((key.as_ref(), value.to_string()));
            }
        }
        let mut seen = HashSet::new();
        let date = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap())
        };
        let mut pending = vec![RangeTask::Search(date("scd"), date("ecd"))];
        while let Some(task) = pending.pop() {
            if client.stopped() {
                return Err(Error::Stop("Stopped while listing search results".into()));
            }
            let (start, mut end) = match task {
                RangeTask::Search(start, end) => (start, end),
                RangeTask::Verify {
                    before,
                    expected,
                    observed,
                } => {
                    if ((seen.len() - before) as u64) < expected
                        || observed.iter().any(|target| !seen.contains(target))
                    {
                        return Err(invalid("Search results changed while splitting dates; the incomplete index was not accepted. Run the source search again"));
                    }
                    continue;
                }
            };
            if start.is_some() && end.is_none() {
                let mut newest_query = base.clone();
                newest_query
                    .iter_mut()
                    .find(|(key, _)| *key == "order")
                    .unwrap()
                    .1 = "date_d".into();
                newest_query.push(("p", "1".into()));
                let newest = fetch(
                    &format!("/ajax/search/{endpoint}/{tag}"),
                    &newest_query,
                    node,
                )?;
                let entries = newest[node]["data"]
                    .as_array()
                    .ok_or_else(|| invalid("Tag-search result schema changed"))?;
                let mut latest = None;
                for item in entries.iter().filter(|item| item["isAdContainer"] != true) {
                    let date = result_date(item)?.ok_or_else(|| {
                        invalid("Search cannot bound its end without creation dates")
                    })?;
                    latest = Some(latest.map_or(date, |last: NaiveDate| last.max(date)));
                }
                let Some(latest) = latest else {
                    if newest[node]["total"] == 0 {
                        continue;
                    }
                    return Err(invalid("Search cannot determine its latest creation date"));
                };
                if start.is_some_and(|start| start > latest) {
                    continue;
                }
                end = Some(latest);
            }
            if let Some((start, end)) = start
                .zip(end)
                .filter(|(start, end)| (*end - *start).num_days() >= 365)
            {
                // Pixiv silently trims long date ranges; query at most 365 inclusive days.
                let split = start + Duration::days((end - start).num_days() / 2);
                let left = RangeTask::Search(Some(start), Some(split));
                let right = RangeTask::Search(
                    Some(
                        split
                            .succ_opt()
                            .ok_or_else(|| invalid("Search date overflow"))?,
                    ),
                    Some(end),
                );
                if base
                    .iter()
                    .any(|(key, value)| *key == "order" && value == "date")
                {
                    pending.extend([right, left]);
                } else {
                    pending.extend([left, right]);
                }
                continue;
            }
            let mut query = base.clone();
            if let Some(start) = start {
                query.push(("scd", start.to_string()));
            }
            if let Some(end) = end {
                query.push(("ecd", end.to_string()));
            }
            query.push(("p", "1".into()));
            let mut found = Vec::new();
            let mut range_seen = HashSet::new();
            let mut dates = Vec::new();
            let mut expected = 0;
            let mut complete = false;
            for reverse in [false, true] {
                let before_reverse = found.len();
                if reverse {
                    if complete || start.is_none() || start != end {
                        break;
                    }
                    // shortcut: two public date windows per day, narrow further if their union is incomplete.
                    let order = query.iter_mut().find(|(key, _)| *key == "order").unwrap();
                    order.1 = if order.1 == "date" { "date_d" } else { "date" }.into();
                }
                let mut page_seen = HashSet::new();
                for page in 1..=MAX_WORKS {
                    if client.stopped() {
                        return Err(Error::Stop("Stopped while listing search results".into()));
                    }
                    query.last_mut().unwrap().1 = page.to_string();
                    let body = fetch(&format!("/ajax/search/{endpoint}/{tag}"), &query, node)?;
                    let result = &body[node];
                    let entries = result["data"]
                        .as_array()
                        .ok_or_else(|| invalid("Tag-search result schema changed"))?;
                    let total = result["total"]
                        .as_u64()
                        .ok_or_else(|| invalid("Missing tag-search total"))?;
                    expected = expected.max(total);
                    if expected > MAX_WORKS as u64 {
                        return Err(invalid(&format!(
                            "Search has {expected} works, exceeding the {MAX_WORKS} work limit; narrow the search"
                        )));
                    }
                    let before = page_seen.len();
                    for item in entries {
                        if item["isAdContainer"] == true {
                            continue;
                        }
                        let id = item["id"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| item["id"].to_string());
                        let target = Target::new(kind, &id)?;
                        page_seen.insert(target.clone());
                        if let Some(date) = result_date(item)? {
                            if start.is_some_and(|start| date < start)
                                || end.is_some_and(|end| date > end)
                            {
                                return Err(invalid(
                                    "Search returned a work outside the requested date range",
                                ));
                            }
                            dates.push(date);
                        }
                        if range_seen.insert(target.clone()) {
                            found.push(target);
                        }
                    }
                    progress(format!(
                        "検索列挙: 確定 {}件 / 今の区間 {}/{}件 · {}〜{} · {page}ページ",
                        refs.len(),
                        found.len(),
                        expected,
                        start.map_or_else(|| "開始指定なし".into(), |d| d.to_string()),
                        end.map_or_else(|| "終了指定なし".into(), |d| d.to_string())
                    ));
                    if found.len() as u64 > expected {
                        return Err(invalid("Tag-search total changed; run the search again"));
                    }
                    if found.len() as u64 == expected {
                        complete = true;
                        break;
                    }
                    if page_seen.len() == before
                        || result["lastPage"]
                            .as_u64()
                            .is_some_and(|last| page as u64 >= last)
                    {
                        break;
                    }
                }
                if reverse {
                    found[before_reverse..].reverse();
                }
            }
            if complete {
                for target in found {
                    if seen.insert(target.clone()) {
                        refs.push(target);
                    }
                }
                if refs.len() > MAX_WORKS {
                    return Err(invalid(&format!(
                        "Search exceeds {MAX_WORKS} works; narrow the search"
                    )));
                }
                continue;
            }
            let first = dates.iter().min().copied().ok_or_else(|| {
                invalid("Search is truncated but has no creation dates to split; narrow the search")
            })?;
            let last = dates.iter().max().copied().unwrap();
            // Unbounded tails stay unbounded; no assumed epoch can omit older works.
            let split = if let Some((start, end)) =
                start.zip(end).filter(|(start, end)| start < end)
            {
                start + Duration::days((end - start).num_days() / 2)
            } else if first < last {
                first + Duration::days((last - first).num_days() / 2)
            } else if end.is_none_or(|end| first < end) {
                first
            } else if start.is_none_or(|start| first > start) {
                first
                    .pred_opt()
                    .ok_or_else(|| invalid("Search date underflow"))?
            } else {
                return Err(invalid(&format!("Search incomplete on {first}: found {} of {expected} {kind} works within the accessible page range, even in both date orders. Anonymous search cannot enumerate this crowded day completely; narrow the source search. No complete selection was accepted", found.len())));
            };
            if start.is_some_and(|start| split < start) || end.is_some_and(|end| split >= end) {
                return Err(invalid("Search date split made no progress"));
            }
            let next = split
                .succ_opt()
                .ok_or_else(|| invalid("Search date overflow"))?;
            pending.push(RangeTask::Verify {
                before: seen.len(),
                expected,
                observed: found,
            });
            let left = RangeTask::Search(start, Some(split));
            let right = RangeTask::Search(Some(next), end);
            if base
                .iter()
                .any(|(key, value)| *key == "order" && value == "date")
            {
                pending.extend([right, left]);
            } else {
                pending.extend([left, right]);
            }
            progress(format!(
                "検索のページ範囲で未達のため日付分割: {split}以前 / {next}以降"
            ));
        }
    }
    Ok(refs)
}

fn result_date(item: &Value) -> Result<Option<NaiveDate>> {
    item["createDate"]
        .as_str()
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|date| {
                    date.with_timezone(&FixedOffset::east_opt(9 * 3600).unwrap())
                        .date_naive()
                })
                .map_err(|_| invalid("Invalid search result creation date"))
        })
        .transpose()
}

#[cfg(test)]
#[path = "search_scale_tests.rs"]
mod scale_tests;

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
    fn query_search_maps_modes_and_keeps_filters_and_literal_keywords() {
        for (mode, expected) in [
            ("tag_tc", "s_tag"),
            ("tag", "s_tag_only"),
            ("tag_full", "s_tag_full"),
            ("text", "s_tc"),
            ("s_tc", "s_tc"),
        ] {
            let client = Search {
                pages: vec![json!({"novel":{"data":[{"id":"1004"}],"total":1}})],
                ..Default::default()
            };
            let refs = discover(
                &client,
                &format!("https://www.pixiv.net/search?q=R18&s_mode={mode}&type=novel&p=3&work_lang=ja&r=1"),
                "all",
            )
            .unwrap();
            assert_eq!(refs, vec![Target::new("novel", "1004").unwrap()]);
            let calls = client.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "/ajax/search/novels/R18");
            for (key, value) in [
                ("word", "R18"),
                ("s_mode", expected),
                ("mode", "safe"),
                ("p", "1"),
                ("work_lang", "ja"),
                ("r", "1"),
            ] {
                assert!(calls[0].1.contains(&(key.into(), value.into())));
            }
            assert!(!calls[0]
                .1
                .iter()
                .any(|(key, _)| matches!(key.as_str(), "q" | "type")));
        }
        for word in ["猫 + 犬", "C++", "a/b&c?d#e%20", "R18 -漫画"] {
            let mut url = Url::parse("https://www.pixiv.net/en/search").unwrap();
            url.query_pairs_mut()
                .append_pair("q", word)
                .append_pair("type", "novel");
            let client = Search {
                pages: vec![json!({"novel":{"data":[],"total":0}})],
                ..Default::default()
            };
            discover(&client, url.as_str(), "all").unwrap();
            let calls = client.calls.lock().unwrap();
            assert!(calls[0].1.contains(&("word".into(), word.into())));
            assert!(calls[0].1.contains(&("s_mode".into(), "s_tag_full".into())));
            assert!(calls[0].0.starts_with("/ajax/search/novels/"));
            assert!(!calls[0].0["/ajax/search/novels/".len()..].contains(['/', '?', '#']));
        }
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
    fn split_results_cannot_drop_parent_count_or_observed_ids() {
        let first = json!({"novel":{"data":[{"id":"1","createDate":"2024-01-01T12:00:00+09:00"}],"total":2,"lastPage":1}});
        for pages in [
            vec![
                first.clone(),
                json!({"novel":{"data":[],"total":0}}),
                json!({"novel":{"data":[{"id":"1","createDate":"2024-01-01T12:00:00+09:00"}],"total":1}}),
            ],
            vec![
                first.clone(),
                json!({"novel":{"data":[{"id":"2","createDate":"2024-01-02T12:00:00+09:00"}],"total":1}}),
                json!({"novel":{"data":[{"id":"2","createDate":"2024-01-02T12:00:00+09:00"}],"total":1}}),
                json!({"novel":{"data":[{"id":"3","createDate":"2024-01-01T12:00:00+09:00"}],"total":1}}),
            ],
        ] {
            let error = discover(
                &Search {
                    pages,
                    ..Default::default()
                },
                "https://www.pixiv.net/tags/test/novels",
                "novel",
            )
            .unwrap_err();
            assert!(error.to_string().contains("changed while splitting"));
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
