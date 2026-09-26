//! Real end-to-end smoke against the Anthropic Messages API. Opt-in:
//! skipped unless `TAU_SMOKE=1` and credentials are present
//! (`ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`), so keyless dev/CI
//! runs stay offline and green.

use futures::StreamExt;
use tau_anthropic::AnthropicModel;
use tau_core::{Message, Model, ModelEvent, Request, StopReason};

fn gated() -> Option<AnthropicModel> {
    if std::env::var("TAU_SMOKE").as_deref() != Ok("1") {
        return None;
    }
    let model = std::env::var("TAU_MODEL")
        .or_else(|_| std::env::var("ANTHROPIC_MODEL"))
        .unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
    AnthropicModel::from_env(model).ok()
}

#[tokio::test]
async fn streams_a_real_response() {
    let Some(model) = gated() else {
        eprintln!("skipping: TAU_SMOKE!=1 or no anthropic credentials");
        return;
    };
    let request = Request {
        messages: vec![Message::user("Reply with exactly the word: ok")],
        ..Request::default()
    };
    let mut text = String::new();
    let mut stop = None;
    let mut stream = model.stream(&request).await;
    while let Some(event) = stream.next().await {
        match event {
            ModelEvent::TextDelta { text: delta } => text.push_str(&delta),
            ModelEvent::Done { stop: s } => stop = Some(s),
            ModelEvent::Error { message } => panic!("stream error: {message}"),
            _ => {}
        }
    }
    assert!(
        text.to_lowercase().contains("ok"),
        "expected 'ok' in response, got: {text:?}"
    );
    assert_eq!(stop, Some(StopReason::Stop), "clean stop, got: {stop:?}");
}
