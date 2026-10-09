//! The optional proxy, end to end over real loopback HTTP: a streamed provider response is
//! forwarded unchanged and measured, and a browser cannot use the proxy. Ported from ProxyRegression.cs.

mod common;

use common::Scratch;
use speedtracker::history::HistoryStore;
use speedtracker::proxy::ProxyService;
use speedtracker::tracker::Tracker;
use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// A stand-in model provider: checks what reached it, then streams four tokens and a usage row
// as server-sent events, 150 ms after the request and 80 ms apart.
fn serve(mut stream: TcpStream, forwarded: &AtomicBool) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let (mut length, mut authorized) = (0usize, false);
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap_or((header, ""));
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap_or(0),
            "authorization" => authorized = value.trim() == "Bearer smoke-only",
            _ => {}
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    if !request.starts_with("POST /v1/chat/completions ") {
        return stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
    let model = serde_json::from_slice::<serde_json::Value>(&body).ok().and_then(|body| body["model"].as_str().map(str::to_string));
    forwarded.store(authorized && model.as_deref() == Some("smoke-model"), Ordering::SeqCst);
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
    let mut send = |event: &str| -> std::io::Result<()> {
        write!(stream, "{:x}\r\n{event}\r\n", event.len())?;
        stream.flush()
    };
    std::thread::sleep(Duration::from_millis(150));
    for _ in 0..4 {
        send("data: {\"model\":\"smoke-model\",\"choices\":[{\"delta\":{\"content\":\"token \"}}]}\n\n")?;
        std::thread::sleep(Duration::from_millis(80));
    }
    send("data: {\"model\":\"smoke-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":40}}\n\ndata: [DONE]\n\n")?;
    stream.write_all(b"0\r\n\r\n")
}

#[test]
fn proxy_forwards_a_streamed_response_measures_it_and_rejects_browser_requests() {
    let files = Scratch::new("proxy");
    let upstream = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
    let upstream_port = upstream.local_addr().unwrap().port();
    let forwarded = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&forwarded);
    std::thread::spawn(move || {
        for stream in upstream.incoming().flatten() {
            let _ = serve(stream, &seen);
        }
    });

    let history = Arc::new(HistoryStore::new(Some(&files.root)));
    let tracker = Tracker::new(Arc::clone(&history), None, Some(Vec::new()), None, Some(Box::new(BTreeSet::new))).expect("tracker starts");
    tracker.list_processes_with(Vec::new);
    // Port 0: any free port.
    let proxy = ProxyService::new(Arc::clone(&tracker), 0, Some(HashMap::from([("smoke".to_string(), format!("http://127.0.0.1:{upstream_port}"))])));
    proxy.start().expect("proxy listens");
    let url = format!("http://127.0.0.1:{}/smoke@opencode/v1/chat/completions", proxy.port());

    // A web page sends Origin; it must not reach a model API through this proxy.
    match ureq::post(&url).set("Origin", "https://untrusted.example").send_string("{}") {
        Err(ureq::Error::Status(status, _)) => assert_eq!(status, 403, "proxy rejects browser Origin"),
        other => panic!("a request with an Origin header must be refused, got {other:?}"),
    }
    assert!(!forwarded.load(Ordering::SeqCst), "the refused request never reached the provider");

    let response = ureq::post(&url).set("Authorization", "Bearer smoke-only").set("Content-Type", "application/json").send_string("{\"model\":\"smoke-model\",\"stream\":true}").expect("the proxied request succeeds");
    assert_eq!(response.status(), 200);
    let reply = response.into_string().expect("the stream is readable");
    assert!(forwarded.load(Ordering::SeqCst), "proxy forwards the request body and Authorization header");
    assert_eq!(reply.matches("\"content\":\"token \"").count(), 4, "every streamed token row arrives unmodified");
    assert!(reply.ends_with("data: [DONE]\n\n"), "the stream is complete to its last event");

    // The record is written just after the last byte is relayed.
    let mut records = Vec::new();
    for _ in 0..40 {
        records = history.read(None).expect("history reads");
        if !records.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(records.len(), 1, "one proxied call, one record");
    let record = &records[0];
    assert_eq!((record.source.as_deref(), record.harness.as_str(), record.model.as_str()), (Some("proxy"), "opencode", "smoke-model"), "the call is attributed from the route tag and the stream");
    assert_eq!((record.output_tokens, record.input_tokens, record.tokens_estimated, record.aborted), (40, Some(10), false, false), "proxy persists the provider's reported usage");
    let line = std::fs::read_to_string(files.path("history.jsonl")).unwrap();
    assert!(!line.contains("smoke-only") && !line.contains("token "), "history holds no credential and no reply text");
    // The provider waits 150 ms before the first token and spends 240 ms between the first and last.
    assert!(record.ttft.is_some_and(|ttft| (0.1..2.0).contains(&ttft)), "first token is timed after the provider's delay: {:?}", record.ttft);
    assert!(record.generation.is_some_and(|generation| generation >= 0.15), "generation spans the streamed tokens: {:?}", record.generation);
    assert!(record.tps.is_some_and(|rate| rate > 0.0));
    assert!(tracker.active().is_empty(), "proxy completion leaves no false live call");
    assert!(tracker.held_rate().is_some_and(|rate| rate > 0.0), "and leaves its speed held");
    proxy.stop();
}
