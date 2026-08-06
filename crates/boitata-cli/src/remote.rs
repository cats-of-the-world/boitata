//! `--remote` mode: schedule a task on a running `boitata-server` and stream its
//! progress, instead of running the agent loop in this process. The CLI is a thin
//! client here — it POSTs the task, tails the server's SSE event stream, prints
//! the final result, and cancels the run on Ctrl-C.

use anyhow::{Context, bail};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

pub async fn run(
    base_url: &str,
    task: String,
    blueprint: Option<String>,
    api_token: Option<String>,
) -> anyhow::Result<()> {
    let base = base_url.trim_end_matches('/').to_string();
    // Don't follow redirects, and bound connect + idle-read time (not *total*
    // request time), so a malicious/compromised --remote server (or a MITM on an
    // http:// URL) can't redirect the CLI at an arbitrary internal endpoint or
    // stall it forever. A total `.timeout()` would be wrong here: the event
    // stream is a long-lived SSE body that legitimately stays open for the whole
    // run, and a total deadline would sever it (and abort the run tail) mid-way.
    // `read_timeout` bounds per-read idle time instead — the server's SSE
    // keep-alives reset it — so a genuinely stalled peer is still caught.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    // Schedule the run.
    let resp = auth(
        client
            .post(format!("{base}/api/runs"))
            .json(&json!({ "task": task, "blueprint": blueprint })),
        &api_token,
    )
    .send()
    .await
    .with_context(|| format!("failed to reach boitata-server at {base}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let msg = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or_else(|| status.to_string());
        bail!("server rejected the task: {msg}");
    }
    let id = resp.json::<Value>().await?["id"]
        .as_str()
        .map(String::from)
        .context("server response missing run id")?;
    // The server controls this value; validate it's a UUID before splicing it
    // into request paths, so a hostile server can't redirect follow-up requests
    // to other endpoints (e.g. an id of "../blueprints").
    Uuid::parse_str(&id).context("server returned a malformed run id")?;
    println!("Scheduled run {id} on {base}");

    // Cancel the run if the user interrupts. Aborted once the run ends so it
    // doesn't linger.
    let canceller = {
        let (base, id, client, api_token) =
            (base.clone(), id.clone(), client.clone(), api_token.clone());
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nInterrupt received; cancelling run…");
                let _ = auth(
                    client.post(format!("{base}/api/runs/{id}/cancel")),
                    &api_token,
                )
                .send()
                .await;
            }
        })
    };

    // Tail the live event stream until a terminal event or the stream closes.
    let resp = auth(
        client.get(format!("{base}/api/runs/{id}/events")),
        &api_token,
    )
    .send()
    .await
    .context("failed to open event stream")?
    .error_for_status()?;
    let mut events = resp.bytes_stream().eventsource();
    while let Some(item) = events.next().await {
        let event = item.context("event stream error")?;
        let Ok(ev) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        let tag = ev["event"].as_str().unwrap_or("");
        println!("{}", format_event(tag, &ev));
        if tag == "run_completed" || tag == "blueprint_completed" {
            break;
        }
    }
    canceller.abort();

    // Fetch and print the final result and derive an exit status.
    let detail = auth(client.get(format!("{base}/api/runs/{id}")), &api_token)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    print_result(&detail);

    match detail["status"]["state"].as_str().unwrap_or("") {
        "succeeded" => Ok(()),
        "cancelled" => bail!("run was cancelled"),
        _ => bail!(
            "{}",
            detail["status"]["error"]
                .as_str()
                .unwrap_or("run did not complete successfully")
        ),
    }
}

/// Attach the bearer token to a request when one is configured (no-op when the
/// server runs without auth). Mirrors `boitata-server`'s `require_token` gate.
fn auth(req: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.header("authorization", format!("Bearer {t}")),
        None => req,
    }
}

/// One-line rendering of a streamed audit event (see `boitata_core::audit`).
fn format_event(tag: &str, ev: &Value) -> String {
    let s = |k: &str| ev[k].as_str().unwrap_or("").to_string();
    let n = |k: &str| ev[k].as_i64().map(|v| v.to_string()).unwrap_or_default();
    match tag {
        "run_started" => format!("▶ run started · {}/{}", s("provider"), s("model")),
        "llm_response" => {
            let tools = ev["tool_calls"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let suffix = if tools.is_empty() {
                String::new()
            } else {
                format!(" · tools: {tools}")
            };
            format!("🧠 iteration {}{suffix}", n("iteration"))
        }
        "tool_call" => format!(
            "{} {}({}) → {}",
            if ev["is_error"].as_bool().unwrap_or(false) {
                "✗"
            } else {
                "✓"
            },
            s("name"),
            truncate(&s("arguments"), 80),
            truncate(&s("result"), 160),
        ),
        "tool_denied" => format!("⛔ {} denied · {}", s("name"), s("reason")),
        "context_compacted" => "🗜 context compacted".to_string(),
        "run_completed" => format!(
            "{} run {} · {} iteration(s)",
            if ev["success"].as_bool().unwrap_or(false) {
                "■"
            } else {
                "✗"
            },
            if ev["success"].as_bool().unwrap_or(false) {
                "completed"
            } else {
                "failed"
            },
            n("iterations"),
        ),
        "blueprint_started" => format!("▶ blueprint {} · entry {}", s("blueprint"), s("entry")),
        "node_executed" => {
            let status = s("status");
            let output = s("output");
            let mut line = format!(
                "◆ [{}] {} → {} ({})",
                s("node"),
                s("kind"),
                s("next"),
                status
            );
            if !output.is_empty() {
                line.push_str(&format!("\n    {}", truncate(&output, 500)));
            }
            line
        }
        "super_step_retried" => format!(
            "↻ retry #{} at step {} · {}",
            n("attempt"),
            n("step"),
            s("error")
        ),
        "blueprint_completed" => format!(
            "■ blueprint finished · {} step(s) · {}",
            n("steps"),
            s("reason")
        ),
        other => format!("• {other}"),
    }
}

/// Print the final transcript / message from a run-detail payload.
fn print_result(detail: &Value) {
    let Some(result) = detail.get("result").filter(|r| !r.is_null()) else {
        return;
    };
    println!("---");
    if let Some(calls) = result["tool_calls"].as_array() {
        for call in calls {
            let marker = if call["is_error"].as_bool().unwrap_or(false) {
                "✗"
            } else {
                "✓"
            };
            println!(
                "{marker} {}({})",
                call["name"].as_str().unwrap_or(""),
                call["arguments"].as_str().unwrap_or("")
            );
        }
    }
    if let Some(transcript) = result["transcript"].as_array() {
        for entry in transcript {
            println!(
                "[{}]\n{}\n",
                entry["node"].as_str().unwrap_or(""),
                entry["text"].as_str().unwrap_or("")
            );
        }
    }
    if let Some(message) = result["final_message"].as_str() {
        println!("\n{message}");
    }
}

fn truncate(s: &str, n: usize) -> String {
    // Truncate on a char boundary so multi-byte arguments/results don't panic.
    match s.char_indices().nth(n) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}
