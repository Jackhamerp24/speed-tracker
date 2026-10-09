//! The proxy's stream meter, fed provider responses shaped as each API documents them: when the
//! first token and first visible text arrived, and the usage the provider reported.

use speedtracker::proxy::StreamMeasurement;

/// Feeds server-sent events, each at the given second.
fn stream(events: &[(f64, &str)]) -> StreamMeasurement {
    let mut meter = StreamMeasurement::new();
    meter.set_content_type("text/event-stream");
    for (time, data) in events {
        meter.ingest(format!("data: {data}\n\n").as_bytes(), *time);
    }
    meter.complete(events.last().map_or(0.0, |(time, _)| *time));
    meter
}

#[test]
fn anthropic_stream_times_thinking_as_first_token_and_text_as_first_visible() {
    let mut meter = StreamMeasurement::new();
    meter.set_content_type("text/event-stream");
    // Anthropic names each event on its own line before the data line.
    meter.ingest(
        b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":25,\"cache_read_input_tokens\":1000,\"output_tokens\":1}}}\n\n",
        0.2,
    );
    for (time, data) in [
        (1.0, r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#),
        (1.2, r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}"#),
        (2.0, r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#),
        (2.1, r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}"#),
        (3.0, r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" world"}}"#),
        (3.1, r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#),
        (3.2, r#"{"type":"message_stop"}"#),
    ] {
        meter.ingest(format!("data: {data}\n\n").as_bytes(), time);
    }
    meter.complete(3.2);
    assert_eq!((meter.format.as_str(), meter.model.as_str(), meter.streamed, meter.failed), ("anthropic", "claude-opus-5-5", true, false));
    assert_eq!(meter.first, Some(1.0), "the thinking block opening is the first token");
    assert_eq!(meter.visible, Some(2.0), "the text block opening is the first visible output");
    assert_eq!(meter.last, Some(3.0), "the last text, not the closing bookkeeping events, ends generation");
    assert_eq!((meter.output(), meter.estimated(), meter.input, meter.cache), (15, false, Some(25), Some(1000)), "usage is the provider's final count");
    // 15 tokens over the two seconds from first token to last text.
    assert_eq!(meter.live_rate(), Some(7.5));
}

#[test]
fn openai_responses_stream_reports_usage_details_and_counts_seen_reasoning_in_the_rate() {
    let completed = r#"{"type":"response.completed","response":{"model":"gpt-5","usage":{"input_tokens":50,"output_tokens":30,"input_tokens_details":{"cached_tokens":20},"output_tokens_details":{"reasoning_tokens":10}}}}"#;
    let meter = stream(&[
        (0.1, r#"{"type":"response.created","response":{"model":"gpt-5","usage":null}}"#),
        (0.8, r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#),
        (1.5, r#"{"type":"response.output_text.delta","delta":"Hi"}"#),
        (2.5, completed),
    ]);
    assert_eq!((meter.format.as_str(), meter.model.as_str()), ("openai-responses", "gpt-5"));
    assert_eq!((meter.first, meter.visible), (Some(0.8), Some(1.5)), "a reasoning item starts generation; text makes it visible");
    assert_eq!((meter.output(), meter.input, meter.cache, meter.reasoning), (30, Some(50), Some(20), Some(10)));
    assert_eq!(meter.rate_tokens(), 30.0, "reasoning that streamed took stream time, so it counts toward the rate");

    // The same usage, but the reasoning never appeared in the stream: it took no stream time.
    let hidden = stream(&[(1.5, r#"{"type":"response.output_text.delta","delta":"Hi"}"#), (2.5, completed)]);
    assert_eq!(hidden.rate_tokens(), 20.0, "hidden reasoning is left out of the rate");
    assert_eq!(hidden.output(), 30, "but stays in the output total");
}

#[test]
fn gemini_stream_adds_thinking_to_output_and_times_the_thought_as_first_token() {
    let meter = stream(&[
        (0.5, r#"{"candidates":[{"content":{"parts":[{"text":"weighing options","thought":true}]}}],"modelVersion":"gemini-3-pro"}"#),
        (1.5, r#"{"candidates":[{"content":{"parts":[{"text":"Answer"}]}}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":30,"thoughtsTokenCount":12}}"#),
    ]);
    assert_eq!((meter.format.as_str(), meter.model.as_str()), ("gemini", "gemini-3-pro"));
    assert_eq!((meter.first, meter.visible), (Some(0.5), Some(1.5)));
    // Gemini reports thinking apart from the reply: 30 + 12.
    assert_eq!((meter.output(), meter.reasoning, meter.input, meter.cache), (42, Some(12), Some(100), Some(40)));
}

#[test]
fn ollama_ndjson_stream_is_read_line_by_line() {
    let mut meter = StreamMeasurement::new();
    meter.set_content_type("application/x-ndjson");
    meter.ingest(b"{\"model\":\"llama4\",\"message\":{\"content\":\"Hi\"},\"done\":false}\n", 0.4);
    meter.ingest(b"{\"model\":\"llama4\",\"message\":{\"content\":\"\"},\"done\":true,\"prompt_eval_count\":7,\"eval_count\":5}\n", 0.9);
    meter.complete(0.9);
    assert_eq!((meter.format.as_str(), meter.model.as_str(), meter.streamed), ("ollama", "llama4", true));
    assert_eq!((meter.first, meter.output(), meter.input), (Some(0.4), 5, Some(7)));
}

#[test]
fn a_whole_json_reply_has_usage_but_no_stream_timing() {
    let mut meter = StreamMeasurement::new();
    meter.set_content_type("application/json; charset=utf-8");
    // One body, arriving in two network reads.
    meter.ingest(b"{\"model\":\"gpt-5\",\"choices\":[{\"message\":{\"content\":\"Hel", 0.5);
    meter.ingest(b"lo there\"}}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":3}}", 0.6);
    assert!(!meter.recognized(), "nothing is known until the body is complete");
    meter.complete(0.9);
    assert_eq!((meter.format.as_str(), meter.streamed), ("openai-chat", false));
    assert_eq!((meter.first, meter.visible), (None, None), "a reply that arrived whole has no first-token time");
    assert_eq!((meter.output(), meter.input, meter.estimated()), (3, Some(9), false));
}

#[test]
fn without_reported_usage_output_is_estimated_at_four_characters_a_token() {
    let forty = "a".repeat(40);
    let meter = stream(&[(1.0, &format!(r#"{{"choices":[{{"delta":{{"content":"{forty}"}}}}]}}"#))]);
    assert!(meter.estimated(), "no usage row means the count is an estimate");
    assert_eq!(meter.output(), 10);
    // A partial token rounds up.
    let meter = stream(&[(1.0, &format!(r#"{{"choices":[{{"delta":{{"content":"{forty}b"}}}}]}}"#))]);
    assert_eq!(meter.output(), 11);
}

#[test]
fn an_error_event_marks_the_call_failed() {
    let meter = stream(&[
        (0.5, r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"par"}}"#),
        (0.7, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
    ]);
    assert!(meter.failed);
    assert_eq!(meter.format, "anthropic", "what was read before the error is kept");
}
