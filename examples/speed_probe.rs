//! Offline performance probe. No Pixiv requests; loopback sockets and synthetic works only.
#![allow(dead_code)]
#[path = "../rust/archive.rs"]
mod archive;
#[path = "../rust/collection.rs"]
mod collection;
#[path = "../rust/error.rs"]
mod error;
#[path = "../rust/jobs.rs"]
mod jobs;
#[path = "../rust/media.rs"]
mod media;
#[path = "../rust/novel.rs"]
mod novel;
#[path = "../rust/target.rs"]
mod target;
mod transport {
    include!("../rust/transport.rs");

    // Private transport seam: production URL validation is NOT changed. Only this
    // example calls open_validated with a literal loopback URL and scaled timing.
    pub fn probe(workers: usize, header_ms: u64, body_ms: u64) -> serde_json::Value {
        let count = 16;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for stream in listener.incoming().take(count) {
                handlers.push(std::thread::spawn(move || {
                    let mut stream = stream.unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut headers = Vec::new();
                    let mut byte = [0];
                    while !headers.ends_with(b"\r\n\r\n") {
                        stream.read_exact(&mut byte).unwrap();
                        headers.push(byte[0]);
                    }
                    std::thread::sleep(Duration::from_millis(header_ms));
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    stream.flush().unwrap();
                    std::thread::sleep(Duration::from_millis(body_ms));
                    stream.write_all(b"{}").unwrap();
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        let client = Arc::new(Online {
            http: Http::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            gate: Mutex::new(Admission::default()),
            wake: Condvar::new(),
            stop: Arc::default(),
            interval: Duration::from_millis(40),
            class_intervals: [Duration::from_millis(40); 2],
            retry_delay: Duration::from_millis(1),
            started: Instant::now(),
            budget: count,
            max_bytes: 1024,
        });
        let started = Instant::now();
        let mut handles = Vec::new();
        for worker in 0..workers {
            let c = client.clone();
            let u = url.clone();
            handles.push(std::thread::spawn(move || {
                for _ in (worker..count).step_by(workers) {
                    let response = c.open_validated(u.clone(), false).unwrap();
                    c.read(response, &mut Vec::new(), 1024, false).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        server.join().unwrap();
        serde_json::json!({"workers":workers,"header_delay_ms":header_ms,"body_delay_ms":body_ms,"requests":client.requests(),"interval_ms":40,"start_spacing_floor_seconds":0.6,"elapsed_seconds":started.elapsed().as_secs_f64(),"explicit_rate_wait_seconds":client.rate_wait().as_secs_f64()})
    }
}
use serde_json::{json, Value};
use std::{fs, sync::Arc};
fn synthetic_novels(count: usize) -> String {
    let base: Value = serde_json::from_str(transport::DEMO).unwrap();
    let mut responses = serde_json::Map::new();
    let mut ids = serde_json::Map::new();
    for i in 0..count {
        let id = (200000 + i).to_string();
        ids.insert(id.clone(), Value::Null);
        let mut body = base["responses"]["/ajax/novel/1004"].clone();
        body["id"] = json!(id);
        body["content"] = json!("合成小説の本文。\n".repeat(256));
        body["textCount"] = json!(2304);
        body["textEmbeddedImages"] = json!({});
        body.as_object_mut().unwrap().remove("coverUrl");
        responses.insert(format!("/ajax/novel/{id}"), body);
    }
    responses.insert(
        "/ajax/user/9001/profile/all".into(),
        json!({"illusts":{},"manga":{},"novels":ids}),
    );
    json!({"synthetic":true,"responses":responses,"assets":{}}).to_string()
}
fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse().unwrap())
        .unwrap_or(4000);
    assert!((1..=10000).contains(&count));
    let temp = tempfile::tempdir().unwrap();
    let fixture = temp.path().join("fixture.json");
    fs::write(&fixture, synthetic_novels(count)).unwrap();
    let mut saves = Vec::new();
    for workers in [1, 4] {
        let opts = jobs::Options {
            targets: vec!["user:9001".into()],
            all: true,
            jobs: workers,
            request_budget: 100000,
            fixture: Some(fixture.clone()),
            out: temp.path().join(format!("archive-{workers}")),
            ..Default::default()
        };
        for pass in ["fresh", "resume"] {
            let report = jobs::run(opts.clone(), false, Arc::default(), |_| {}).unwrap();
            assert_eq!(report["status"], "complete");
            assert_eq!(
                report["collection"]["problems"].as_array().unwrap().len(),
                0
            );
            if pass == "fresh" {
                assert_eq!(report["counts"]["complete"], count);
            } else {
                assert_eq!(report["already_complete"], count);
                assert_eq!(report["requests"], 1);
            }
            saves.push(json!({"workers":workers,"pass":pass,"works":count,"requests":report["requests"],"elapsed_seconds":report["elapsed_seconds"],"collection_copied":report["collection"]["copied"],"collection_existing":report["collection"]["existing"]}));
        }
    }
    let mut sockets = Vec::new();
    for (header_ms, body_ms) in [(1, 0), (80, 0), (1, 80)] {
        for workers in [1, 4] {
            for _ in 0..3 {
                sockets.push(transport::probe(workers, header_ms, body_ms));
            }
        }
    }
    println!("{}",serde_json::to_string_pretty(&json!({"scope":"Linux loopback HTTP (not TLS), 40 ms scaled limiter; production offline archive/collection code; synthetic novels without covers, 6400 UTF-8 bytes of body each; warm caches/order uncontrolled; no Windows or live Pixiv measurement", "version":env!("CARGO_PKG_VERSION"),"archive_runs":saves,"transport_runs":sockets})).unwrap());
}
