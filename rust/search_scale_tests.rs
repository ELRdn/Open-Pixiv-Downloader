use super::*;
use serde_json::json;
use std::{ops::RangeInclusive, sync::Mutex};

const SOURCE: &str = "https://www.pixiv.net/tags/scale/novels";

#[derive(Debug)]
struct Call {
    start: Option<NaiveDate>,
    end: Option<NaiveDate>,
    page: usize,
}

struct CappedSearch {
    first_day: NaiveDate,
    total: usize,
    per_day: usize,
    create_dates: Vec<String>,
    ignore_dates: bool,
    step_days: i64,
    clamp_year: bool,
    page_size: usize,
    calls: Mutex<Vec<Call>>,
}

impl CappedSearch {
    fn new(first_day: &str, total: usize, per_day: usize) -> Self {
        assert!(per_day > 0 && total <= MAX_WORKS);
        let first_day = NaiveDate::parse_from_str(first_day, "%Y-%m-%d").unwrap();
        Self {
            first_day,
            total,
            per_day,
            create_dates: (0..total.div_ceil(per_day))
                .map(|day| {
                    // Just after JST midnight also catches accidental UTC date filtering.
                    format!("{}T00:30:00+09:00", first_day + Duration::days(day as i64))
                })
                .collect(),
            ignore_dates: false,
            step_days: 1,
            clamp_year: false,
            page_size: 30,
            calls: Mutex::default(),
        }
    }

    fn sparse_year_clamp(mut self) -> Self {
        self.step_days = 4;
        self.clamp_year = true;
        self.create_dates = (0..self.total.div_ceil(self.per_day))
            .map(|day| {
                format!(
                    "{}T00:30:00+09:00",
                    self.first_day + Duration::days(day as i64 * self.step_days)
                )
            })
            .collect();
        self
    }

    fn assert_request_limits(&self) {
        let calls = self.calls.lock().unwrap();
        assert!(calls.len() < 25_000, "{} search requests", calls.len());
        assert!(calls.iter().all(|call| (1..=10).contains(&call.page)));
    }
}

impl Transport for CappedSearch {
    fn api(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        assert_eq!(path, "/ajax/search/novels/scale", "discovery only");
        let parameter = |name| {
            query
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
        };
        let date = |name| {
            parameter(name).map(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap())
        };
        let (start, end) = (date("scd"), date("ecd"));
        let page = parameter("p").unwrap().parse::<usize>().unwrap();
        assert!(page > 0);
        {
            let mut calls = self.calls.lock().unwrap();
            assert!(calls.len() + 1 < 25_000, "search request budget exceeded");
            calls.push(Call { start, end, page });
        }
        let index = |days: i64| (days * self.per_day as i64).clamp(0, self.total as i64) as usize;
        let effective_start = if self.clamp_year {
            start.map(|start| {
                start.max(
                    end.unwrap_or(
                        self.first_day
                            + Duration::days((self.create_dates.len() as i64 - 1) * self.step_days),
                    ) - Duration::days(364),
                )
            })
        } else {
            start
        };
        let (lo, hi) = if self.ignore_dates {
            (0, self.total)
        } else {
            (
                effective_start.map_or(0, |day| {
                    let days = (day - self.first_day).num_days();
                    index(
                        days.div_euclid(self.step_days)
                            + i64::from(days.rem_euclid(self.step_days) != 0),
                    )
                }),
                end.map_or(self.total, |day| {
                    index((day - self.first_day).num_days().div_euclid(self.step_days) + 1)
                }),
            )
        };
        let total = hi.saturating_sub(lo);
        let offset = (page.min(10) - 1) * self.page_size;
        let ascending = parameter("order") == Some("date");
        let data: Vec<_> = (offset..(offset + self.page_size).min(total))
            .map(|position| {
                let index = if ascending {
                    lo + position
                } else {
                    hi - 1 - position
                };
                json!({
                    "id": (index + 1).to_string(),
                    "createDate": self.create_dates[index / self.per_day]
                })
            })
            .collect();
        Ok(json!({"novel": {
            "data": data, "total": total, "lastPage": total.div_ceil(self.page_size).min(10)
        }}))
    }

    fn download(&self, _: &str, _: &std::path::Path) -> Result<()> {
        panic!("search discovery must not download media")
    }

    fn requests(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn stopped(&self) -> bool {
        false
    }

    fn cancel(&self) {}
}

fn assert_ids(targets: &[Target], mut expected: RangeInclusive<usize>, order: &str) {
    assert_eq!(targets.len(), expected.clone().count());
    for target in targets {
        assert_eq!(target.kind, "novel");
        let id = if order == "date" {
            expected.next()
        } else {
            expected.next_back()
        };
        assert_eq!(target.id.parse::<usize>().unwrap(), id.unwrap());
    }
}

#[test]
fn fake_has_thirty_entry_pages_inclusive_dates_and_repeats_page_ten() {
    let client = CappedSearch::new("2024-02-20", 600, 20);
    let fetch = |page: &str, order: &str, bounds: &[(&str, String)]| {
        let mut query = vec![("p", page.into()), ("order", order.into())];
        query.extend_from_slice(bounds);
        client.api("/ajax/search/novels/scale", &query).unwrap()["novel"].clone()
    };
    let tenth = fetch("10", "date", &[]);
    assert_eq!(tenth["total"], 600);
    assert_eq!(tenth["lastPage"], 10);
    assert_eq!(tenth["data"].as_array().unwrap().len(), 30);
    assert_eq!(tenth["data"][0]["id"], "271");
    assert_eq!(tenth["data"][29]["id"], "300");
    assert_eq!(fetch("11", "date", &[]), tenth);
    assert_eq!(fetch("1", "date_d", &[])["data"][0]["id"], "600");
    let leap_day = fetch(
        "1",
        "date",
        &[("scd", "2024-02-29".into()), ("ecd", "2024-02-29".into())],
    );
    assert_eq!(leap_day["total"], 20);
    assert_eq!(leap_day["lastPage"], 1);
    assert_eq!(leap_day["data"][0]["id"], "181");
    assert_eq!(leap_day["data"][19]["id"], "200");
    assert_eq!(
        leap_day["data"][0]["createDate"],
        "2024-02-29T00:30:00+09:00"
    );
    let empty = fetch("1", "date", &[("ecd", "2024-02-19".into())]);
    assert_eq!(empty["total"], 0);
    assert_eq!(empty["lastPage"], 0);
    assert!(empty["data"].as_array().unwrap().is_empty());
}

#[test]
fn capped_search_enumerates_524_163727_and_164000_ids_exactly_once() {
    for total in [524, 163_727, 164_000] {
        let client = CappedSearch::new("2010-01-01", total, 40);
        let targets = discover(&client, SOURCE, "novel").unwrap();
        assert_ids(&targets, 1..=total, "date_d");
        client.assert_request_limits();
        let calls = client.calls.lock().unwrap();
        assert!(calls.iter().any(|call| call.page == 10));
        let partitions: HashSet<_> = calls.iter().map(|call| (call.start, call.end)).collect();
        assert!(partitions.len() > if total == 524 { 1 } else { 10 });
    }
}

#[test]
fn original_sized_search_over_a_million_still_splits_and_enumerates_every_id() {
    let mut client = CappedSearch::new("2010-01-01", 1_512_674, 500);
    // Larger fixture pages keep the million-ID regression test bounded in time.
    client.page_size = 30_000;
    let targets = discover(&client, SOURCE, "novel").unwrap();
    assert_ids(&targets, 1..=client.total, "date_d");
    client.assert_request_limits();
    assert!(client
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|call| call.start.is_some()));
}

#[test]
fn unbounded_tails_have_no_assumed_epoch_in_either_order() {
    for first_day in ["1800-01-01", "2400-01-01"] {
        for order in ["date", "date_d"] {
            let client = CappedSearch::new(first_day, 701, 20);
            let targets = discover(&client, &format!("{SOURCE}?order={order}"), "novel").unwrap();
            assert_ids(&targets, 1..=701, order);
            client.assert_request_limits();
            let calls = client.calls.lock().unwrap();
            assert!(calls[0].start.is_none() && calls[0].end.is_none());
            assert!(calls
                .iter()
                .any(|call| call.start.is_none() && call.end.is_some()));
            assert!(calls
                .iter()
                .any(|call| call.start.is_some() && call.end.is_some()));
            assert!(calls
                .iter()
                .all(|call| call.start.is_none() || call.end.is_some()));
        }
    }
}

#[test]
fn user_date_bounds_and_leap_day_are_inclusive_in_either_order() {
    for order in ["date", "date_d"] {
        for (start, end, ids) in [
            (Some("2024-02-25"), Some("2024-03-05"), 401..=800),
            (Some("2024-02-29"), Some("2024-02-29"), 561..=600),
            (Some("2024-02-29"), None, 561..=1300),
            (None, Some("2024-02-29"), 1..=600),
        ] {
            let client = CappedSearch::new("2024-02-15", 1300, 40);
            let mut source = format!("{SOURCE}?order={order}");
            for (key, value) in [("scd", start), ("ecd", end)] {
                if let Some(value) = value {
                    source.push_str(&format!("&{key}={value}"));
                }
            }
            let targets = discover(&client, &source, "novel").unwrap();
            assert_ids(&targets, ids, order);
            client.assert_request_limits();
            for call in client.calls.lock().unwrap().iter() {
                if let Some(start) = start {
                    let start = NaiveDate::parse_from_str(start, "%Y-%m-%d").unwrap();
                    assert!(
                        call.start.is_some_and(|day| day >= start)
                            || (end.is_none() && call.start.is_none() && call.end.is_none())
                    );
                }
                if let Some(end) = end {
                    let end = NaiveDate::parse_from_str(end, "%Y-%m-%d").unwrap();
                    assert!(call.end.is_some_and(|day| day <= end));
                }
            }
        }
    }
}

#[test]
fn both_date_windows_cover_a_crowded_day_without_losing_order_or_ids() {
    for order in ["date", "date_d"] {
        for total in [301, 315, 600] {
            for bounds in ["", "&scd=2024-02-29&ecd=2024-02-29"] {
                let client = CappedSearch::new("2024-02-29", total, total);
                let targets =
                    discover(&client, &format!("{SOURCE}?order={order}{bounds}"), "novel").unwrap();
                assert_ids(&targets, 1..=total, order);
                client.assert_request_limits();
            }
        }
    }
}

#[test]
fn more_than_both_date_windows_on_one_day_is_explicitly_incomplete() {
    for order in ["date", "date_d"] {
        for bounds in ["", "&scd=2024-02-29&ecd=2024-02-29"] {
            let client = CappedSearch::new("2024-02-29", 601, 601);
            let error = discover(&client, &format!("{SOURCE}?order={order}{bounds}"), "novel")
                .unwrap_err()
                .to_string();
            for detail in ["incomplete", "2024-02-29", "600", "601"] {
                assert!(error.contains(detail), "{error}");
            }
            client.assert_request_limits();
        }
    }
}

#[test]
fn server_ignoring_partition_date_filters_is_rejected() {
    for order in ["date", "date_d"] {
        let mut client = CappedSearch::new("2024-02-15", 524, 40);
        client.ignore_dates = true;
        let error = discover(&client, &format!("{SOURCE}?order={order}"), "novel")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("outside") && error.contains("date range"),
            "{error}"
        );
        client.assert_request_limits();
    }
}

#[test]
fn long_date_ranges_and_open_end_do_not_lose_works_when_server_clamps_to_a_year() {
    for order in ["date", "date_d"] {
        for filters in ["", "&scd=2010-01-01", "&scd=2010-01-01&ecd=2020-12-31"] {
            let client = CappedSearch::new("2010-01-01", 524, 1).sparse_year_clamp();
            let targets = discover(
                &client,
                &format!("{SOURCE}?order={order}{filters}"),
                "novel",
            )
            .unwrap();
            assert_ids(&targets, 1..=524, order);
            for call in client.calls.lock().unwrap().iter() {
                if let Some(start) = call.start {
                    assert!(
                        (call.end.expect("explicit end avoids server clipping") - start).num_days()
                            < 365
                    );
                }
            }
            client.assert_request_limits();
        }
    }
}

#[test]
fn invalid_or_reversed_dates_are_rejected_before_any_request() {
    for filters in [
        "scd=not-a-date",
        "ecd=2023-02-29",
        "scd=2024-02-30",
        "ecd=2024-13-01",
        "scd=2024-03-01&ecd=2024-02-29",
        "scd=2024-02-29&ecd=2024-02-28",
    ] {
        let client = CappedSearch::new("2024-02-29", 1, 1);
        assert!(
            matches!(
                discover(&client, &format!("{SOURCE}?{filters}"), "novel"),
                Err(Error::Invalid(_))
            ),
            "{filters}"
        );
        assert_eq!(client.requests(), 0, "{filters}");
    }
}
