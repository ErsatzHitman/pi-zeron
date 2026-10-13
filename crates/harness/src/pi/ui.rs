use super::{normalize::string, rpc::Client};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::{sync::oneshot, task::JoinSet};
use zeron_proto::{UserInputAnswer, UserInputQuestion};
pub(super) type Input =
    dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync;
#[derive(Default)]
pub(super) struct Dialogs {
    pub input: Option<Arc<Input>>,
    tasks: JoinSet<()>,
    /// Dialogs Pi is still waiting on, so a cancel can answer them: Pi's
    /// `editor` has no timeout and an unanswered one stalls `abort`.
    open: Arc<std::sync::Mutex<Vec<(Value, Client)>>>,
    /// Secret questions by question id, answered directly (see
    /// [`Dialogs::answer_secret`]) so the secret never enters the chat's
    /// synced command history.
    secrets: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}
impl Dialogs {
    pub fn request(&mut self, client: Client, frame: &Value) {
        let method = string(frame, "method").to_owned();
        if !matches!(method.as_str(), "select" | "confirm" | "input" | "editor") {
            return;
        }
        while self.tasks.try_join_next().is_some() {}
        let id = frame["id"].clone();
        let Some(input) = &self.input else {
            let _ = client.send(json!({"type":"extension_ui_response","id":id,"cancelled":true}));
            return;
        };
        let options = if method == "confirm" {
            vec!["Yes".into(), "No".into()]
        } else {
            frame["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        };
        let title = string(frame, "title");
        let message = string(frame, "message");
        let question = UserInputQuestion {
            id: uuid::Uuid::new_v4().to_string(),
            header: "Pi".into(),
            question: if message.is_empty() {
                title.into()
            } else {
                format!("{title}\n{message}")
            },
            options: options.clone(),
            multi_select: false,
            prefill: frame["prefill"].as_str().map(str::to_owned),
            multiline: method == "editor",
            secret: method == "input" && super::piui::asks_for_secret(frame),
        };
        let question_id = question.id.clone();
        if question.secret {
            lock(&self.secrets).push((question_id.clone(), id.clone()));
        }
        let answer = (input)(vec![question]);
        let timeout = frame["timeout"].as_u64();
        lock(&self.open).push((id.clone(), client.clone()));
        let open = self.open.clone();
        let secrets = self.secrets.clone();
        self.tasks.spawn(async move {
            let deadline = async {
                match timeout {
                    Some(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                    None => std::future::pending().await,
                }
            };
            let answers =
                tokio::select! { result=answer=>result.unwrap_or_default(),_=deadline=>vec![] };
            let value = answers
                .iter()
                .find(|a| a.question_id == question_id)
                .and_then(|a| a.labels.first());
            // The secret came (or comes) on its own; this is its placeholder.
            if value.is_some_and(|value| value == zeron_proto::SECRET_ANSWER_SENT) {
                return;
            }
            lock(&secrets).retain(|(secret, _)| *secret != question_id);
            let mut response = json!({"type":"extension_ui_response","id":id});
            match (method.as_str(), value) {
                ("confirm", Some(value)) if options.contains(value) => {
                    response["confirmed"] = json!(value == "Yes")
                }
                ("select", Some(value)) if options.contains(value) => {
                    response["value"] = json!(value)
                }
                ("input" | "editor", Some(value)) => response["value"] = json!(value),
                _ => response["cancelled"] = json!(true),
            }
            lock(&open).retain(|(open_id, _)| *open_id != id);
            let _ = client.send(response);
        });
    }
    /// Answer a secret question with what the user typed, sent apart from
    /// the chat's history. `false` when no such question is waiting.
    pub fn answer_secret(&mut self, client: &Client, question_id: &str, value: &str) -> bool {
        let pi_id = {
            let mut secrets = lock(&self.secrets);
            let Some(at) = secrets.iter().position(|(id, _)| id == question_id) else {
                return false;
            };
            secrets.remove(at).1
        };
        lock(&self.open).retain(|(open_id, _)| *open_id != pi_id);
        let _ = client.send(json!({"type":"extension_ui_response","id":pi_id,"value":value}));
        true
    }
    pub fn cancel(&mut self) {
        self.tasks.abort_all();
        lock(&self.secrets).clear();
        for (id, client) in lock(&self.open).drain(..) {
            let _ = client.send(json!({"type":"extension_ui_response","id":id,"cancelled":true}));
        }
    }
}
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn timeout_cancels_instead_of_selecting_and_drops_the_input_receiver() {
        let (writer, peer) = tokio::io::duplex(4096);
        let (_peer_writer, reader) = tokio::io::duplex(4096);
        let transport = super::super::rpc::Transport::new(writer, reader);
        let (bridge, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let mut dialogs = Dialogs::default();
        dialogs.input = Some(Arc::new(move |_| {
            let (tx, rx) = oneshot::channel();
            bridge.send(tx).unwrap();
            rx
        }));
        dialogs.request(
            transport.client.clone(),
            &json!({
                "id":"expired", "method":"select", "options":["first"], "timeout":10
            }),
        );
        let mut sender = requests.recv().await.unwrap();
        let mut lines = BufReader::new(peer).lines();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap(),
            json!({
                "type":"extension_ui_response", "id":"expired", "cancelled":true
            })
        );
        sender.closed().await;
        dialogs.request(
            transport.client.clone(),
            &json!({"id":"stopped", "method":"input"}),
        );
        let mut sender = requests.recv().await.unwrap();
        dialogs.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), sender.closed())
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap(),
            json!({
                "type":"extension_ui_response", "id":"stopped", "cancelled":true
            })
        );
    }

    #[tokio::test]
    async fn a_credential_input_during_a_run_asks_a_secret_question() {
        let (writer, _peer) = tokio::io::duplex(4096);
        let (_peer_writer, reader) = tokio::io::duplex(4096);
        let transport = super::super::rpc::Transport::new(writer, reader);
        let (bridge, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let mut dialogs = Dialogs::default();
        dialogs.input = Some(Arc::new(move |questions: Vec<UserInputQuestion>| {
            bridge.send(questions[0].secret).unwrap();
            oneshot::channel().1
        }));
        let ask =
            |dialogs: &mut Dialogs, frame: Value| dialogs.request(transport.client.clone(), &frame);
        ask(
            &mut dialogs,
            json!({"id":"k", "method":"input", "title":"RunInfra API key", "placeholder":"rk_..."}),
        );
        ask(
            &mut dialogs,
            json!({"id":"n", "method":"input", "title":"Session name"}),
        );
        ask(
            &mut dialogs,
            json!({"id":"s", "method":"select", "title":"Pick an API key", "options":["a"]}),
        );
        assert_eq!(asked.recv().await, Some(true));
        assert_eq!(asked.recv().await, Some(false));
        assert_eq!(asked.recv().await, Some(false), "only free text is masked");
        dialogs.cancel();
    }

    /// The secret reaches Pi on its own; the history's placeholder answer
    /// never does.
    #[tokio::test]
    async fn a_secret_is_answered_directly_and_its_placeholder_is_dropped() {
        let (writer, peer) = tokio::io::duplex(4096);
        let (_peer_writer, reader) = tokio::io::duplex(4096);
        let transport = super::super::rpc::Transport::new(writer, reader);
        let (bridge, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let mut dialogs = Dialogs::default();
        dialogs.input = Some(Arc::new(move |questions: Vec<UserInputQuestion>| {
            let (tx, rx) = oneshot::channel();
            bridge.send((questions[0].id.clone(), tx)).unwrap();
            rx
        }));
        dialogs.request(
            transport.client.clone(),
            &json!({"id":"k", "method":"input", "title":"OpenRouter API key"}),
        );
        let (question, answer) = asked.recv().await.unwrap();
        assert!(dialogs.answer_secret(&transport.client, &question, "sk-or-v1-x"));
        assert!(!dialogs.answer_secret(&transport.client, &question, "again"));
        answer
            .send(vec![zeron_proto::UserInputAnswer {
                question_id: question,
                labels: vec![zeron_proto::SECRET_ANSWER_SENT.into()],
            }])
            .unwrap();
        let mut lines = BufReader::new(peer).lines();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap(),
            json!({"type":"extension_ui_response", "id":"k", "value":"sk-or-v1-x"})
        );
        let more =
            tokio::time::timeout(std::time::Duration::from_millis(200), lines.next_line()).await;
        assert!(more.is_err(), "the placeholder is never sent to Pi");
    }
}
