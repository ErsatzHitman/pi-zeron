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
/// A user message's `timestamp`, as Pi stamps it: unique per message.
fn stamp() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_790_000_000_000);
    NEXT.fetch_add(1, Ordering::SeqCst)
}
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
    message_err(text, stop, "mock provider failure", None);
}
fn message_err(text: &str, stop: &str, error: &str, provider: Option<&str>) {
    let mut message = json!({"role":"assistant","content":[{"type":"text","text":text}],"stopReason":stop,"errorMessage":error,"usage":{"input":12,"output":3}});
    if let Some(provider) = provider {
        message["provider"] = json!(provider);
    }
    emit(json!({"type":"message_end","message":message}));
}
/// The failures a prompt of that name makes the provider report: the error
/// text and the provider it came from.
fn provider_failure(text: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match text {
        "quota" => ("insufficient_quota: You exceeded your current quota", Some("openai")),
        "auth" => ("No API key found for anthropic", Some("anthropic")),
        "limit" => ("429 rate limit exceeded, retry after 30 seconds", Some("anthropic")),
        "overload" => ("529 overloaded_error", Some("anthropic")),
        _ => return None,
    })
}
/// PIUI emulation (the user's `00-piui.ts` hub): one envelope per notify,
/// a process-wide `agentSeq`, and nothing published before `hello`.
struct PiUi {
    on: AtomicBool,
    seq: std::sync::atomic::AtomicU64,
}
static PIUI: PiUi = PiUi {
    on: AtomicBool::new(false),
    seq: std::sync::atomic::AtomicU64::new(0),
};
const INSTANCE: &str = "fixture-instance";
fn piui(body: Value) {
    if !PIUI.on.load(Ordering::SeqCst) {
        return;
    }
    let mut envelope = json!({"v":1,"agentSeq":PIUI.seq.fetch_add(1, Ordering::SeqCst) + 1,"instance":INSTANCE});
    for (key, value) in body.as_object().unwrap() {
        envelope[key] = value.clone();
    }
    piui_notify(&envelope.to_string());
}
fn piui_notify(message: &str) {
    emit(json!({"type":"extension_ui_request","id":uuid(),"method":"notify","message":format!("PIUI {message}"),"notifyType":"info"}));
}
fn uuid() -> String {
    format!("ui-{}", PIUI.seq.load(Ordering::SeqCst))
}
fn ask_user_sheet() -> Value {
    json!({"id":"question","ns":"ask-user","kind":"panel","placement":"sheet","title":"Pick a color",
        "sections":[{"kind":"form","id":"ask-form","fields":[
            {"id":"selection","kind":"select","searchable":true,"options":[
                {"value":"Red","label":"Red"},{"value":"Blue","label":"Blue","description":"calm"}]},
            {"id":"freeform","kind":"text","placeholder":"Type something"}],
            "actions":[{"id":"submit","label":"Submit","variant":"primary"}]}],
        "actions":[{"id":"cancel","label":"Cancel"}]})
}
/// `/pi_ui_event <base64url JSON>`: the client's half of the protocol.
fn piui_event(args: &str) {
    use base64::Engine as _;
    let event: Value = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(args.trim())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("piui-events.jsonl")
        .unwrap();
    writeln!(log, "{event}").unwrap();
    match event["type"].as_str().unwrap_or("action") {
        "hello" => {
            PIUI.on.store(true, Ordering::SeqCst);
            piui(json!({"op":"hello","protocol":1,"kinds":["panel","roster","status"],"sectionKinds":["form","checklist"]}));
            piui(json!({"op":"reset","reason":"snapshot"}));
            piui(json!({"op":"set","el":ask_user_sheet()}));
            piui(json!({"op":"set","el":{"id":"jobs","ns":"bash","kind":"roster","placement":"pinned",
                "title":"Background bash · 1 running",
                "rows":[{"id":"j1","label":"npm test","state":"running","detail":"3s",
                    "actions":[{"id":"kill","label":"Kill","variant":"danger","confirm":"Kill this job?"}]}]}}));
            // As bash-background sends it: `active` is the terminal's manage mode.
            piui(json!({"op":"channel","channel":"bash-bg:fleet","payload":{"entries":[
                {"key":"j1","label":"npm test","state":"running"}],"active":false,"selected":"j1"}}));
            // A large element arrives in UTF-8-safe chunks without agentSeq.
            let big = json!({"v":1,"agentSeq":PIUI.seq.fetch_add(1, Ordering::SeqCst) + 1,"instance":INSTANCE,
                "op":"set","el":{"id":"list","ns":"todo","kind":"panel","placement":"pinned",
                "title":"Todos 0/1","sections":[{"kind":"checklist","id":"tasks",
                "items":[{"id":"1","text":"ñ".repeat(40),"state":"active"}]}]}})
            .to_string();
            let mid = big.char_indices().nth(big.chars().count() / 2).unwrap().0;
            let (first, second) = big.split_at(mid);
            for (i, data) in [(1, second), (0, first)] {
                piui_notify(&json!({"v":1,"op":"chunk","id":"c1","i":i,"n":2,"data":data}).to_string());
            }
        }
        "bye" => PIUI.on.store(false, Ordering::SeqCst),
        "snapshot" => piui(json!({"op":"reset","reason":"snapshot"})),
        _ => match (event["elementId"].as_str(), event["actionId"].as_str()) {
            (Some("ask-user:question"), Some("submit" | "cancel")) => {
                piui(json!({"op":"remove","ns":"ask-user","id":"question"}))
            }
            (Some("btw:thread"), Some("submit")) => {
                let question = event["value"]["fields"]["q"].as_str().unwrap_or("").to_owned();
                piui(btw_thread(json!([
                    {"role":"user","text":"why is the sky blue?"},
                    {"role":"assistant","text":"**Rayleigh scattering**: shorter wavelengths scatter more."},
                    {"role":"user","text":question},
                    {"role":"assistant","text":format!("btw:{question}")}])));
            }
            (Some("btw:thread"), Some("close")) => {
                piui(json!({"op":"remove","ns":"btw","id":"thread"}))
            }
            (Some("bash:jobs"), Some("kill")) => {
                piui(json!({"op":"patch","ns":"bash","id":"jobs","patch":{"title":"Background bash · 0 running",
                    "rows":[{"id":"j1","label":"npm test","state":"error","detail":"killed"}]}}));
                piui(json!({"op":"channel","channel":"bash-bg:fleet","payload":{"entries":[],"active":false,"selected":""}}));
            }
            _ => {}
        },
    }
}
/// The btw side thread, as a screen with a pinned question box.
fn btw_thread(turns: Value) -> Value {
    json!({"op":"set","el":{"id":"thread","ns":"btw","kind":"panel","placement":"screen","title":"btw",
        "sections":[
            {"kind":"meta","id":"btw-meta","text":"mock/mock · medium"},
            {"kind":"turns","id":"btw-transcript","turns":turns},
            {"kind":"form","id":"btw-composer","placement":"pinned",
             "fields":[{"id":"q","kind":"text","placeholder":"Ask a side question"}],
             "actions":[{"id":"submit","label":"Send","variant":"primary"}]}],
        "actions":[{"id":"close","label":"Close"}]}})
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
                    {"name":"noisy","description":"says what it did","source":"extension","sourceInfo":{"path":"noisy.ts"}},
                    {"name":"pi_ui_event","description":"Internal: PIUI events","source":"extension","sourceInfo":{"path":"00-piui.ts"}},
                    {"name":"skill:probe","description":"skill","source":"skill","sourceInfo":{"path":"SKILL.md"}}
                ]}),
            ),
            "prompt" => {
                let mut text = v["message"].as_str().unwrap_or("").to_owned();
                if text == "env" {
                    text = format!("env:{}", std::env::var_os("CLAUDECODE").is_some());
                }
                if text == "args" {
                    text = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
                }
                if text == "guard-env" {
                    let var = |key| std::env::var(key).unwrap_or_else(|_| "-".into());
                    text = format!(
                        "on={} file={} force={} ext={} args={} usage={}",
                        var("ZERON_DELETE_GUARD"),
                        var("ZERON_DELETE_GUARD_FILE"),
                        var("ZERON_DELETE_GUARD_FORCE"),
                        var("ZERON_DELETE_GUARD_EXT"),
                        std::env::args().skip(1).collect::<Vec<_>>().join(" "),
                        var("ZERON_USAGE_ALERTS_FILE"),
                    );
                }
                if text == "which-model" {
                    text = format!("{model}/{thinking}");
                }
                if text == "reject" || text == "reject-auth" {
                    let error = if text == "reject" {
                        "preflight rejected"
                    } else {
                        "No API key found for anthropic"
                    };
                    emit(
                        json!({"type":"response","id":v["id"],"command":"prompt","success":false,"error":error}),
                    );
                    continue;
                }
                if text == "/noop" || text == "handled" {
                    disposition(&v, "handled");
                    continue;
                }
                // An extension command that reports what it did, as Jev's
                // `/jev off` does.
                if let Some(args) = text.strip_prefix("/noisy ") {
                    emit(
                        json!({"type":"extension_ui_request","id":uuid(),"method":"notify",
                        "message":format!("noisy: {args}"),"notifyType":"info"}),
                    );
                    disposition(&v, "handled");
                    continue;
                }
                // Extension commands run at once, even mid-turn, and start no run.
                if let Some(args) = text.strip_prefix("/pi_ui_event ") {
                    piui_event(args);
                    disposition(&v, "handled");
                    continue;
                }
                if text == "plain-ui" {
                    for request in [
                        json!({"method":"notify","message":"Saved the plan","notifyType":"warning"}),
                        json!({"method":"setStatus","statusKey":"!mode","statusText":"Plan"}),
                        json!({"method":"setWidget","widgetKey":"btw","widgetLines":["q: why?","a: because"],"widgetPlacement":"aboveEditor"}),
                        json!({"method":"setTitle","title":"Pi fixture"}),
                        json!({"method":"set_editor_text","text":"rewritten prompt"}),
                    ] {
                        let mut request = request;
                        request["type"] = json!("extension_ui_request");
                        request["id"] = json!(uuid());
                        emit(request);
                    }
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
                // The out-of-memory killer's death: a signal, not an exit code.
                #[cfg(unix)]
                if text == "sigkill" {
                    eprintln!("mock memory diagnostic");
                    // SAFETY: signals this process only.
                    unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
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
                    json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":text}],"timestamp":stamp()}}),
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
                        if text == "custom" {
                            // Extensions deliver results as custom messages.
                            for (kind, display) in [("workflow-result", true), ("jev-decompose", false)] {
                                let message = json!({"role":"custom","customType":kind,"display":display,
                                    "details":{"status":"completed"},"content":[{"type":"text","text":"# Done\nAll steps passed"}]});
                                emit(json!({"type":"message_start","message":message}));
                                emit(json!({"type":"message_end","message":message}));
                            }
                        }
                        if abort.load(Ordering::SeqCst) {
                            message("", "aborted");
                        } else if text == "error" {
                            message("", "error");
                        } else if let Some((error, provider)) = provider_failure(&text) {
                            message_err("", "error", error, provider);
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
                                    json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":next}],"timestamp":stamp()}}),
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
                        if text == "piui-btw" {
                            std::thread::spawn(|| {
                                std::thread::sleep(Duration::from_millis(30));
                                piui(btw_thread(json!([
                                    {"role":"user","text":"why is the sky blue?"},
                                    {"role":"assistant","text":"**Rayleigh scattering**: shorter wavelengths scatter more."}])));
                            });
                        }
                        if text == "piui-idle" {
                            // Fleets and timers publish between turns.
                            std::thread::spawn(|| {
                                std::thread::sleep(Duration::from_millis(30));
                                piui(json!({"op":"set","el":{"id":"working","ns":"uniichat","kind":"status",
                                    "placement":"status","text":"uniichat: folding 2 messages into the tree…"}}));
                            });
                        }
                        if text == "idle-dialog" {
                            // A background extension asks with a plain dialog.
                            std::thread::spawn(|| {
                                std::thread::sleep(Duration::from_millis(30));
                                emit(json!({"type":"extension_ui_request","id":"idle","method":"select",
                                    "title":"Keep the worktree?","options":["Keep","Remove"]}));
                            });
                        }
                        // Dies while parked, with the turn long finished.
                        #[cfg(unix)]
                        if text == "idle-sigkill" {
                            std::thread::spawn(|| {
                                std::thread::sleep(Duration::from_millis(50));
                                // SAFETY: signals this process only.
                                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                            });
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
                    json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":v["message"]}],"timestamp":stamp()}}),
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
            "extension_ui_response" if v["id"] == "idle" => {
                let mut log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("dialog-responses.jsonl")
                    .unwrap();
                writeln!(log, "{v}").unwrap();
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
