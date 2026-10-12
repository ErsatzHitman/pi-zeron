//! Stateful cross-platform Pi RPC peer; never built into the application.
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").unwrap();
    out.flush().unwrap();
}
fn response(v: &Value, data: Value) {
    emit(json!({"type":"response","id":v["id"],"command":v["type"],"success":true,"data":data}));
}
/// Pi 1.1 acknowledges setters with no `data` key at all.
fn ack(v: &Value) {
    emit(json!({"type":"response","id":v["id"],"command":v["type"],"success":true}));
}
fn disposition(v: &Value, disposition: &str) {
    response(v, json!({ "disposition": disposition }));
}
fn message(text: &str, stop: &str) {
    emit(
        json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":text}],"stopReason":stop,"errorMessage":"mock provider failure","usage":{"input":12,"output":3}}}),
    );
}
fn queue_update(queue: &std::collections::VecDeque<String>) {
    emit(json!({"type":"queue_update","steering":queue,"followUp":[]}));
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("1.1.0");
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(30));
        std::process::exit(99);
    });
    let file = args
        .iter()
        .position(|a| a == "--session")
        .map(|i| args[i + 1].clone())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap()
                .join("fixture-session.jsonl")
                .display()
                .to_string()
        });
    let session = std::fs::read_to_string(&file)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.lines().next()?).ok())
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "pi-fixture-session".into());
    if !args.iter().any(|a| a == "--no-session") && !std::path::Path::new(&file).exists() {
        std::fs::write(
            &file,
            format!(
                "{}\n",
                json!({"type":"session","id":session,"cwd":std::env::current_dir().unwrap()})
            ),
        )
        .unwrap();
    }
    if !args.iter().any(|a| a == "--no-session") {
        std::fs::write("pi.pid", std::process::id().to_string()).unwrap();
    }
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::<String>::new()));
    let active = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let steering_all = Arc::new(AtomicBool::new(false));
    // Pi answers `abort` only once the run has settled.
    let pending_abort = Arc::new(Mutex::new(None::<Value>));
    let messages = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut dialog = None;
    let mut model = "mock".to_string();
    let mut thinking = "medium".to_string();
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match v["type"].as_str().unwrap_or("") {
            "get_state" => response(
                &v,
                json!({
                    "sessionId":session,"sessionFile":file,
                    "isStreaming":active.load(Ordering::SeqCst),"isCompacting":false,
                    "model":{"id":model,"provider":"mock","name":"Mock","contextWindow":128000,"reasoning":true},
                    "thinkingLevel":thinking,
                    "steeringMode":if steering_all.load(Ordering::SeqCst) {"all"} else {"one-at-a-time"},
                    "followUpMode":"one-at-a-time",
                    "autoCompactionEnabled":true,
                    "messageCount":messages.load(Ordering::SeqCst),
                    "pendingMessageCount":queue.lock().unwrap().len(),
                }),
            ),
            "get_available_models" => response(
                &v,
                json!({"models":[{"id":"mock","provider":"mock","name":"Mock","reasoning":true,"contextWindow":128000},{"id":"mock-2","provider":"mock","name":"Mock 2","reasoning":true,"contextWindow":128000}]}),
            ),
            "get_available_thinking_levels" => {
                response(&v, json!({"levels":["off","low","medium","high"]}))
            }
            "set_model" => {
                model = v["modelId"].as_str().unwrap().into();
                response(
                    &v,
                    json!({"id":model,"provider":"mock","name":model,"reasoning":true,"contextWindow":128000}),
                );
            }
            "set_thinking_level" => {
                thinking = v["level"].as_str().unwrap().into();
                ack(&v);
            }
            "set_steering_mode" => {
                steering_all.store(v["mode"] == "all", Ordering::SeqCst);
                // Recorded so tests can prove when Zeron leaves the mode alone.
                std::fs::write("steering-mode", v["mode"].as_str().unwrap_or("")).unwrap();
                ack(&v);
            }
            "get_commands" => response(
                &v,
                json!({"commands":[
                    {"name":"noop","description":"handled","source":"extension","sourceInfo":{"path":"noop.ts"}},
                    {"name":"pi_ui_event","description":"Internal: PIUI events","source":"extension","sourceInfo":{"path":"00-piui.ts"}},
                    {"name":"skill:probe","description":"skill","source":"skill","sourceInfo":{"path":"SKILL.md"}}
                ]}),
            ),
            "prompt" => {
                let mut text = v["message"].as_str().unwrap_or("").to_owned();
                if text == "env" {
                    text = format!("env:{}", std::env::var_os("CLAUDECODE").is_some());
                }
                if text == "which-model" {
                    text = format!("{model}/{thinking}");
                }
                if text == "reject" {
                    emit(
                        json!({"type":"response","id":v["id"],"command":"prompt","success":false,"error":"preflight rejected"}),
                    );
                    continue;
                }
                if text == "/noop" || text == "handled" {
                    disposition(&v, "handled");
                    continue;
                }
                if text.starts_with("/dialog") {
                    dialog = Some(v.clone());
                    let method = text.split_whitespace().nth(1).unwrap_or("input");
                    let mut request = json!({"type":"extension_ui_request","id":"question","method":method,"title":"Choose"});
                    match method {
                        "select" => request["options"] = json!(["first","second"]),
                        "confirm" => request["message"] = json!("Are you sure?"),
                        "editor" => request["prefill"] = json!("initial\nvalue"),
                        _ => request["placeholder"] = json!("type here"),
                    }
                    emit(request);
                    continue;
                }
                #[cfg(unix)]
                if matches!(text.as_str(), "tree" | "inherited-pipe-crash") {
                    let child = std::process::Command::new("sh")
                        .args(["-c", "sleep 30"])
                        .spawn()
                        .unwrap();
                    std::fs::write("tool.pid", child.id().to_string()).unwrap();
                    if text == "inherited-pipe-crash" {
                        eprintln!("inherited pipe diagnostic");
                        std::process::exit(7);
                    }
                }
                if text == "crash" {
                    eprintln!("mock crash diagnostic");
                    std::process::exit(7);
                }
                let mut queued = queue.lock().unwrap();
                if active.load(Ordering::SeqCst) {
                    queued.push_back(text);
                    queue_update(&queued);
                    disposition(&v, "queued");
                    std::fs::write("pending-steers.json", serde_json::to_vec(&*queued).unwrap())
                        .unwrap();
                    continue;
                }
                active.store(true, Ordering::SeqCst);
                drop(queued);
                abort.store(false, Ordering::SeqCst);
                messages.fetch_add(1, Ordering::SeqCst);
                disposition(&v, "started");
                emit(json!({"type":"agent_start"}));
                emit(
                    json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":text}]}}),
                );
                let active = active.clone();
                let abort = abort.clone();
                let queue = queue.clone();
                let steering_all = steering_all.clone();
                let pending_abort = pending_abort.clone();
                std::thread::spawn(move || {
                    let mut text = text;
                    loop {
                        if text == "burst-start" {
                            emit(json!({"type":"message_start","message":{"role":"assistant"}}));
                            emit(
                                json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"waiting"}}),
                            );
                            while !std::path::Path::new("release-burst").exists()
                                && !abort.load(Ordering::SeqCst)
                            {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                        }
                        if text == "tree" {
                            emit(
                                json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"tree ready"}}),
                            );
                        }
                        if text == "slow" || text == "tree" {
                            for _ in 0..100 {
                                if abort.load(Ordering::SeqCst) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(10));
                            }
                        }
                        if text == "retry" {
                            message("", "error");
                            emit(json!({"type":"agent_end","messages":[],"willRetry":true}));
                            emit(
                                json!({"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":150,"errorMessage":"mock provider failure"}),
                            );
                            std::thread::sleep(Duration::from_millis(150));
                            emit(json!({"type":"agent_start"}));
                        }
                        if text == "compact" {
                            emit(json!({"type":"agent_end","messages":[],"willRetry":false}));
                            emit(json!({"type":"compaction_start","reason":"threshold"}));
                            std::thread::sleep(Duration::from_millis(100));
                            emit(
                                json!({"type":"compaction_end","reason":"threshold","result":{"estimatedTokensAfter":9},"aborted":false,"willRetry":false}),
                            );
                        }
                        if text == "edit" {
                            let args = json!({"path":"a.txt","edits":[{"oldText":"one","newText":"uno"},{"oldText":"two","newText":"dos"}]});
                            emit(
                                json!({"type":"tool_execution_start","toolCallId":"edit-1","toolName":"edit","args":args}),
                            );
                            emit(
                                json!({"type":"tool_execution_end","toolCallId":"edit-1","toolName":"edit","isError":false,"durationMs":3,"result":{"content":[{"type":"text","text":"Edited a.txt"}],"details":{"diff":"-one\n+uno"}}}),
                            );
                        }
                        if abort.load(Ordering::SeqCst) {
                            message("", "aborted");
                        } else if text == "error" {
                            message("", "error");
                        } else {
                            emit(json!({"type":"message_start","message":{"role":"assistant"}}));
                            emit(
                                json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"reply:"}}),
                            );
                            message(&format!("reply:{text}"), "stop");
                        }
                        emit(json!({"type":"agent_end","messages":[],"willRetry":false}));
                        if text == "retry" {
                            emit(json!({"type":"auto_retry_end","success":true,"attempt":1}));
                        }
                        let mut queued = queue.lock().unwrap();
                        if !abort.load(Ordering::SeqCst) && !queued.is_empty() {
                            let count = if steering_all.load(Ordering::SeqCst) {
                                queued.len()
                            } else {
                                1
                            };
                            let mut batch = Vec::new();
                            for _ in 0..count {
                                let next = queued.pop_front().unwrap();
                                queue_update(&queued);
                                emit(
                                    json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":next}]}}),
                                );
                                batch.push(next);
                            }
                            text = batch.join("\n");
                            continue;
                        }
                        active.store(false, Ordering::SeqCst);
                        emit(
                            json!({"type":"agent_settled","aborted":abort.load(Ordering::SeqCst)}),
                        );
                        if let Some(abort) = pending_abort.lock().unwrap().take() {
                            ack(&abort);
                        }
                        if text == "late-notify" {
                            std::thread::spawn(|| {
                                std::thread::sleep(Duration::from_millis(30));
                                emit(
                                    json!({"type":"extension_ui_request","id":"late","method":"notify","message":"background notice","notifyType":"info"}),
                                );
                            });
                        }
                        break;
                    }
                });
            }
            "steer" => {
                response(&v, json!({}));
                emit(
                    json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":v["message"]}]}}),
                );
            }
            "clear_queue" => {
                queue.lock().unwrap().clear();
                response(&v, json!({"steering":[],"followUp":[]}));
            }
            "abort" => {
                abort.store(true, Ordering::SeqCst);
                let mut pending = pending_abort.lock().unwrap();
                if active.load(Ordering::SeqCst) {
                    *pending = Some(v.clone());
                } else {
                    drop(pending);
                    ack(&v);
                }
            }
            "extension_ui_response" => {
                if let Some(prompt) = dialog.take() {
                    emit(
                        json!({"type":"extension_ui_request","id":"notice","method":"notify","message":v.to_string(),"notifyType":"info"}),
                    );
                    disposition(&prompt, "handled");
                }
            }
            "get_entries" => response(&v, json!({"entries":[],"leafId":null})),
            "set_follow_up_mode" | "set_auto_compaction" | "set_session_name" | "compact" => {
                ack(&v)
            }
            "get_session_stats" => response(&v, json!({"sessionId":session,"totalMessages":0})),
            "export_html" => response(&v, json!({"path":"session.html"})),
            other => emit(
                json!({"type":"response","id":v["id"],"command":other,"success":false,"error":format!("Unknown command: {other}")}),
            ),
        }
    }
}
