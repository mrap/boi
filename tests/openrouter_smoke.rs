/// Live smoke test for OpenRouterRuntime.
///
/// Gated behind OPENROUTER_API_KEY — skips silently when the key is absent.
/// Run with:
///   OPENROUTER_API_KEY=sk-... cargo test --test openrouter_smoke -- --nocapture
use std::time::Duration;

use boi::runtime::{openrouter::OpenRouterRuntime, PhaseRuntime};

#[test]
fn smoke_openrouter_live() {
    let key = match std::env::var("OPENROUTER_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("SKIP: OPENROUTER_API_KEY not set — skipping live smoke test");
            return;
        }
    };

    let runtime = OpenRouterRuntime::new();

    let prompt = "Reply with exactly one word: hello";
    let model = "gemini-flash";
    let timeout = Duration::from_secs(30);

    let start = std::time::Instant::now();
    let out = runtime
        .execute(prompt, model, timeout)
        .expect("OpenRouter call failed — check OPENROUTER_API_KEY and network");
    let elapsed_ms = start.elapsed().as_millis();

    // Core assertions
    assert!(!out.text.is_empty(), "response text must not be empty");
    assert!(
        out.input_tokens.unwrap_or(0) > 0,
        "input_tokens must be non-zero; got {:?}",
        out.input_tokens
    );
    assert!(
        out.output_tokens.unwrap_or(0) > 0,
        "output_tokens must be non-zero; got {:?}",
        out.output_tokens
    );
    assert!(out.duration_ms > 0, "duration_ms must be non-zero");

    println!("=== OpenRouter smoke result ===");
    println!("model:          {model}");
    println!("prompt:         {prompt:?}");
    println!("response:       {:?}", out.text);
    println!("input_tokens:   {:?}", out.input_tokens);
    println!("output_tokens:  {:?}", out.output_tokens);
    println!("cost_usd:       {:?}", out.cost_usd);
    println!("duration_ms:    {}", out.duration_ms);
    println!("wall_ms:        {elapsed_ms}");
    println!(
        "api_key_prefix: {}...",
        &key[..key.len().min(8)]
    );
}
