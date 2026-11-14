use crate::{
    error::{invalid, Error, Result},
    target::{Target, MAX_WORKS},
    transport::Transport,
};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use url::Url;

enum RangeTask {
    Search(Option<NaiveDate>, Option<NaiveDate>, String),
    Verify {
        before: usize,
        expected: u64,
        observed: Vec<Target>,
        sort_from: Option<usize>,
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
    mut fetch: impl FnMut(&str, &[(&str, String)], Option<&str>) -> Result<Value>,
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
        let word = base
            .iter()
            .find(|(key, _)| *key == "word")
            .unwrap()
            .1
            .clone();
        let mut seen = HashSet::new();
        let mut times: HashMap<Target, i64> = HashMap::new();
        let mut page_size = 0;
        let date = |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap())
        };
        let mut pending = vec![RangeTask::Search(date("scd"), date("ecd"), word.clone())];
        while let Some(task) = pending.pop() {
            if client.stopped() {
                return Err(Error::Stop("Stopped while listing search results".into()));
            }
            let (start, mut end, search_word) = match task {
                RangeTask::Search(start, end, word) => (start, end, word),
                RangeTask::Verify {
                    before,
                    expected,
                    observed,
                    sort_from,
                } => {
                    if ((seen.len() - before) as u64) != expected
                        || observed.iter().any(|target| !seen.contains(target))
                    {
                        return Err(invalid("Search results changed while splitting dates; the incomplete index was not accepted. Run the source search again"));
                    }
                    if let Some(index) = sort_from {
                        sort_results(
                            &mut refs[index..],
                            &mut times,
                            base.iter()
                                .any(|(key, value)| *key == "order" && value == "date_d"),
                        );
                    }
                    continue;
                }
            };
            let mut base = base.clone();
            base.iter_mut().find(|(key, _)| *key == "word").unwrap().1 = search_word.clone();
            let path = if search_word == word {
                format!("/ajax/search/{endpoint}/{tag}")
            } else {
                search_path(endpoint, &search_word)
            };
            if start.is_some() && end.is_none() {
                let mut newest_query = base.clone();
                newest_query
                    .iter_mut()
                    .find(|(key, _)| *key == "order")
                    .unwrap()
                    .1 = "date_d".into();
                newest_query.push(("p", "1".into()));
                let newest = fetch(&path, &newest_query, Some(node))?;
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
                let left = RangeTask::Search(Some(start), Some(split), search_word.clone());
                let right = RangeTask::Search(
                    Some(
                        split
                            .succ_opt()
                            .ok_or_else(|| invalid("Search date overflow"))?,
                    ),
                    Some(end),
                    search_word,
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
            let mut tags = BTreeMap::new();
            let mut dates = Vec::new();
            let mut expected = 0;
            let mut complete = false;
            let mut gaps = Vec::new();
            let mut searched_slots = 0;
            for reverse in [false, true] {
                let before_reverse = found.len();
                if reverse {
                    if complete || start.is_none() || start != end {
                        break;
                    }
                    let order = query.iter_mut().find(|(key, _)| *key == "order").unwrap();
                    order.1 = if order.1 == "date" { "date_d" } else { "date" }.into();
                }
                let mut page_seen = HashSet::new();
                let mut spans = Vec::new();
                for page in 1..=MAX_WORKS {
                    if client.stopped() {
                        return Err(Error::Stop("Stopped while listing search results".into()));
                    }
                    query.last_mut().unwrap().1 = page.to_string();
                    let body = fetch(&path, &query, Some(node))?;
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
                    let mut span = Vec::new();
                    for item in entries {
                        if item["isAdContainer"] == true {
                            continue;
                        }
                        let id = item["id"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| item["id"].to_string());
                        let target = Target::new(kind, &id)?;
                        if let Ok(id) = id.parse::<u64>() {
                            span.push(id);
                        }
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
                            for tag in item["tags"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .collect::<HashSet<_>>()
                            {
                                *tags.entry(tag.to_owned()).or_default() += 1;
                            }
                            if search_word != word || (start.is_some() && start == end) {
                                let time = item["createDate"]
                                    .as_str()
                                    .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
                                    .ok_or_else(|| {
                                        invalid("Keyword-split result lacks a valid creation time")
                                    })?;
                                times.insert(target.clone(), time.timestamp());
                            }
                            found.push(target);
                        }
                    }
                    page_size = page_size.max(span.len());
                    spans.push(span);
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
                searched_slots += spans.len() * page_size;
                for (index, span) in spans.iter().enumerate() {
                    let remaining = expected.saturating_sub((index * page_size) as u64);
                    if span.len() as u64 >= remaining.min(page_size as u64) {
                        continue;
                    }
                    let mut ids = span.clone();
                    if let Some(id) = index.checked_sub(1).and_then(|i| spans[i].last()) {
                        ids.push(*id);
                    }
                    if let Some(id) = spans.get(index + 1).and_then(|span| span.first()) {
                        ids.push(*id);
                    }
                    // shortcut: scan adjacent visible ID bounds; expand if a hidden work falls outside them.
                    if let (Some(&lo), Some(&hi)) = (ids.iter().min(), ids.iter().max()) {
                        let margin = if index == 0 || index + 1 == spans.len() {
                            (hi - lo).max(32)
                        } else {
                            0
                        };
                        gaps.push((lo.saturating_sub(margin).max(1), hi.saturating_add(margin)));
                    }
                }
            }
            if let Some(day) = start.filter(|_| {
                !complete && kind == "novel" && start == end && expected <= searched_slots as u64
            }) {
                let known = range_seen.union(&seen).cloned().collect();
                progress(format!(
                    "密集日 {day}: 不足 {}件を周辺IDから補完中",
                    expected as usize - found.len()
                ));
                let mut checked = 0_usize;
                let recovered = recover_gaps(
                    &word,
                    &query,
                    day,
                    &gaps,
                    &known,
                    expected as usize - found.len(),
                    &mut |path, query, node| {
                        let body = fetch(path, query, node)?;
                        checked += 1;
                        if checked.is_multiple_of(100) {
                            progress(format!("密集日 {day}: ID補完で{checked}件を確認"));
                        }
                        Ok(body)
                    },
                )?;
                let recovered_any = !recovered.is_empty();
                for (target, time) in recovered {
                    if range_seen.insert(target.clone()) {
                        times.insert(target.clone(), time);
                        found.push(target);
                    }
                }
                if recovered_any && search_word == word {
                    sort_results(
                        &mut found,
                        &mut times,
                        base.iter()
                            .any(|(key, value)| *key == "order" && value == "date_d"),
                    );
                }
                complete = found.len() as u64 == expected;
            }
            if complete {
                if let Some(day) = start.filter(|_| start == end) {
                    if !gaps.is_empty() {
                        progress(format!(
                            "密集日 {day}: ID補完後 {}/{}件",
                            found.len(),
                            expected
                        ));
                    }
                }
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
                if let Some((include, exclude)) = split_keywords(
                    endpoint,
                    node,
                    &query,
                    &tags,
                    found.len(),
                    expected,
                    &mut fetch,
                )? {
                    progress(format!(
                        "密集日 {first}: {expected}件を検索語で分割（元の条件を保持）"
                    ));
                    pending.push(RangeTask::Verify {
                        before: seen.len(),
                        expected,
                        observed: found,
                        sort_from: (search_word == word).then_some(refs.len()),
                    });
                    pending.extend([
                        RangeTask::Search(start, end, exclude),
                        RangeTask::Search(start, end, include),
                    ]);
                    continue;
                }
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
                sort_from: None,
            });
            let left = RangeTask::Search(start, Some(split), search_word.clone());
            let right = RangeTask::Search(Some(next), end, search_word);
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

fn search_path(endpoint: &str, word: &str) -> String {
    let mut url = Url::parse("https://www.pixiv.net/ajax/search/").unwrap();
    url.path_segments_mut()
        .unwrap()
        .pop_if_empty()
        .push(endpoint)
        .push(word);
    url.path().into()
}

fn sort_results(items: &mut [Target], times: &mut HashMap<Target, i64>, descending: bool) {
    items.sort_by(|a, b| {
        times[a]
            .cmp(&times[b])
            .then_with(|| a.id.len().cmp(&b.id.len()))
            .then_with(|| a.id.cmp(&b.id))
    });
    if descending {
        items.reverse();
    }
    for target in items {
        times.remove(target);
    }
}

fn split_keywords(
    endpoint: &str,
    node: &str,
    query: &[(&str, String)],
    tags: &BTreeMap<String, usize>,
    found: usize,
    expected: u64,
    fetch: &mut impl FnMut(&str, &[(&str, String)], Option<&str>) -> Result<Value>,
) -> Result<Option<(String, String)>> {
    let parent = &query.iter().find(|(key, _)| *key == "word").unwrap().1;
    let mut candidates = tags
        .iter()
        .filter(|(tag, _)| {
            !tag.is_empty()
                && tag.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !matches!(tag.as_str(), "AND" | "OR" | "NOT")
                && !parent
                    .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')'))
                    .any(|term| term.trim_start_matches('-') == tag.as_str())
        })
        .collect::<Vec<_>>();
    candidates
        .sort_by_key(|(_, count)| std::cmp::Reverse((**count).min(found.saturating_sub(**count))));
    for (tag, _) in candidates {
        // Spaces inside parentheses are required by Pixiv's query parser.
        let include = format!("( {parent} ) {tag}");
        let exclude = format!("( {parent} ) -{tag}");
        let mut query = query.to_vec();
        query.last_mut().unwrap().1 = "1".into();
        query.iter_mut().find(|(key, _)| *key == "word").unwrap().1 = include.clone();
        let positive = fetch(&search_path(endpoint, &include), &query, Some(node))?;
        let positive = positive[node]["total"]
            .as_u64()
            .ok_or_else(|| invalid("Missing keyword-split total"))?;
        if positive == 0 || positive == expected {
            continue;
        }
        query.iter_mut().find(|(key, _)| *key == "word").unwrap().1 = exclude.clone();
        let negative = fetch(&search_path(endpoint, &exclude), &query, Some(node))?;
        if positive.checked_add(
            negative[node]["total"]
                .as_u64()
                .ok_or_else(|| invalid("Missing keyword-split total"))?,
        ) != Some(expected)
        {
            return Err(invalid("Search results changed while splitting keywords; the incomplete index was not accepted"));
        }
        return Ok(Some((include, exclude)));
    }
    Ok(None)
}

fn recover_gaps(
    source: &str,
    query: &[(&str, String)],
    day: NaiveDate,
    gaps: &[(u64, u64)],
    known: &HashSet<Target>,
    needed: usize,
    fetch: &mut impl FnMut(&str, &[(&str, String)], Option<&str>) -> Result<Value>,
) -> Result<Vec<(Target, i64)>> {
    let parameter = |name| {
        query
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    };
    if parameter("s_mode") != Some("s_tag_full")
        || source.is_empty()
        || !source.chars().all(|c| c.is_alphanumeric() || c == '_')
        || query.iter().any(|(key, _)| {
            ![
                "word", "order", "mode", "p", "scd", "ecd", "s_mode", "r", "lang", "gs", "csw",
            ]
            .contains(key)
        })
    {
        return Ok(Vec::new());
    }
    let word = parameter("word").unwrap();
    let terms = word
        .split_whitespace()
        .map(|term| term.trim_matches(['(', ')']))
        .filter(|term| !term.is_empty() && *term != source)
        .collect::<Vec<_>>();
    let mut windows = gaps.to_vec();
    windows.sort_unstable();
    let mut checked = None;
    let mut recovered = Vec::new();
    for (lo, hi) in windows {
        let lo = checked.map_or(lo, |last: u64| lo.max(last.saturating_add(1)));
        for id in lo..=hi {
            checked = Some(id);
            let target = Target::new("novel", &id.to_string())?;
            if known.contains(&target) {
                continue;
            }
            let body = fetch(&format!("/ajax/novel/{id}"), &[], None)?;
            if body["missing"] == true {
                continue;
            }
            if body["id"] != target.id {
                return Err(invalid("Gap recovery returned another novel ID"));
            }
            if result_date(&body)? != Some(day) {
                continue;
            }
            if parameter("mode") == Some("safe")
                && body["xRestrict"] != 0
                && body["xRestrict"] != "0"
            {
                continue;
            }
            let tags = body["tags"]["tags"]
                .as_array()
                .ok_or_else(|| invalid("Gap recovery needs novel tags"))?;
            let has = |word: &str| {
                tags.iter()
                    .filter_map(|tag| tag["tag"].as_str())
                    .any(|tag| tag.eq_ignore_ascii_case(word))
            };
            // Pixiv's オリジナル search also matches the original-work flag.
            if !has(source) && !(source == "オリジナル" && body["isOriginal"] == true) {
                continue;
            }
            if !terms.iter().all(|term| {
                term.strip_prefix('-')
                    .map_or_else(|| has(term), |term| !has(term))
            }) {
                continue;
            }
            let time = body["createDate"]
                .as_str()
                .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
                .ok_or_else(|| invalid("Gap recovery needs a creation time"))?
                .timestamp();
            recovered.push((target, time));
            if recovered.len() == needed {
                return Ok(recovered);
            }
        }
    }
    Ok(recovered)
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
    fn keyword_splits_reject_unsafe_terms_ignored_conditions_and_changed_counts() {
        let query = [("word", "original".into()), ("p", "1".into())];
        let unsafe_tags = [
            "original",
            "AND",
            "OR",
            "NOT",
            "two words",
            "x) OR y",
            "-exclude",
            "a*b",
            "a:b",
            "a\\b",
            "a\"b",
        ]
        .into_iter()
        .map(|tag| (tag.to_owned(), 300))
        .collect();
        let client = Search::default();
        assert!(split_keywords(
            "novels",
            "novel",
            &query,
            &unsafe_tags,
            600,
            939,
            &mut |path, query, _| client.api(path, query)
        )
        .unwrap()
        .is_none());
        assert_eq!(client.requests(), 0);
        let tags = BTreeMap::from([("safe_tag".into(), 300)]);
        let client = Search {
            pages: vec![json!({"novel":{"data":[],"total":939}})],
            ..Default::default()
        };
        assert!(split_keywords(
            "novels",
            "novel",
            &query,
            &tags,
            600,
            939,
            &mut |path, query, _| client.api(path, query)
        )
        .unwrap()
        .is_none());
        assert_eq!(client.requests(), 1);
        let client = Search {
            pages: vec![
                json!({"novel":{"data":[],"total":300}}),
                json!({"novel":{"data":[],"total":640}}),
            ],
            ..Default::default()
        };
        let error = split_keywords(
            "novels",
            "novel",
            &query,
            &tags,
            600,
            939,
            &mut |path, query, _| client.api(path, query),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("changed while splitting keywords"));
    }

    #[test]
    fn gap_recovery_preserves_original_tag_age_day_and_negative_conditions() {
        let query = [
            ("word", "( オリジナル ) -excluded".into()),
            ("s_mode", "s_tag_full".into()),
            ("mode", "safe".into()),
        ];
        let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let page = |id: &str, date: &str, age, original, tags| json!({"id":id,"createDate":date,"xRestrict":age,"isOriginal":original,"tags":{"tags":tags}});
        let client = Search {
            pages: vec![
                json!({"missing":true}),
                page("3", "2024-03-01T00:00:00+09:00", 0, true, json!([])),
                page("4", "2024-02-29T00:00:00+09:00", 1, true, json!([])),
                page("5", "2024-02-29T00:00:00+09:00", 0, false, json!([])),
                page(
                    "6",
                    "2024-02-29T00:00:00+09:00",
                    0,
                    true,
                    json!([{"tag":"excluded"}]),
                ),
                page("7", "2024-02-28T15:30:00Z", 0, true, json!([])),
            ],
            ..Default::default()
        };
        let recovered = recover_gaps(
            "オリジナル",
            &query,
            day,
            &[(1, 10), (2, 10)],
            &HashSet::from([Target::new("novel", "1").unwrap()]),
            1,
            &mut |path, query, _| client.api(path, query),
        )
        .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].0.label(), "novel:7");
        assert_eq!(client.requests(), 6);
        let mut unsupported = query.to_vec();
        unsupported.push(("work_lang", "ja".into()));
        assert!(recover_gaps(
            "オリジナル",
            &unsupported,
            day,
            &[(1, 10)],
            &HashSet::new(),
            1,
            &mut |_, _, _| panic!("unsupported filters must not be broadened")
        )
        .unwrap()
        .is_empty());
        assert!(matches!(
            recover_gaps(
                "オリジナル",
                &query,
                day,
                &[(1, u64::MAX)],
                &HashSet::new(),
                1,
                &mut |_, _, _| Err(Error::Budget)
            ),
            Err(Error::Budget)
        ));
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
