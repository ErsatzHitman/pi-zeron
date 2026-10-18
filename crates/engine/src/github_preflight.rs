//! GitHub preflight: before a writer starts in a checkout, can it push its
//! branch, and will CI see the push?
//!
//! A writer pushes with git's credentials and then reads CI with `gh`, two
//! different logins, so both are probed: `git push --dry-run` (which asks the
//! remote for write access and changes nothing) and `gh api` for the account,
//! its token's scopes, the repository's permissions, whether Actions is on,
//! and whether the runners the workflows ask for exist. The workflow files of
//! the checkout are read for their triggers, because a push to a new branch
//! starts no CI at all when every workflow waits for a pull request, and the
//! last runs of the default branch are read for a billing block (a failed
//! payment or a spending limit fails every job at once and looks like a
//! broken build). Nothing here prompts or mutates, and a check that cannot be
//! made reads as "unknown", never as ready.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::source_control::{
    ProcessOutput, ProcessRequest, ProcessRunError, ProcessRunner, SystemProcessRunner,
    github_request,
};

/// One `gh api` call.
const CALL_TIMEOUT: Duration = Duration::from_secs(12);
/// The push probe contacts the git remote, which can be slow to refuse.
const PUSH_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const OUTPUT_LIMIT: usize = 512 * 1024;
/// A report is reused this long for the same repository and branch prefix.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// The branch a writer would push when the caller names none.
pub const DEFAULT_BRANCH: &str = "zeron/preflight";
/// The ref the push probe asks about. `--dry-run` never creates it.
const PROBE_REF: &str = "refs/heads/zeron/preflight-probe";
const MAX_WORKFLOW_FILES: usize = 64;
const MAX_WORKFLOW_BYTES: u64 = 256 * 1024;
/// How many of the default branch's latest runs are read for a billing block.
const BILLING_RUNS: usize = 3;

// ───────────────────────────── the report

/// Whether a workflow would start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tri {
    Yes,
    No,
    Unknown,
}

/// Something found wrong, or worth knowing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    /// Stable, for callers that act on one kind of problem.
    pub code: &'static str,
    pub message: String,
}

fn issue(code: &'static str, message: impl Into<String>) -> Issue {
    Issue {
        code,
        message: message.into(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub login: Option<String>,
    /// A classic token's scopes; empty for a fine-grained one, which sends
    /// none.
    pub scopes: Vec<String>,
    /// `classic`, `fineGrained` or `unknown`.
    pub token_kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Permission {
    /// What the `gh` login may do in the repository.
    pub gh_push: Option<bool>,
    /// Whether git's own credentials were accepted for a push.
    pub git_push: Option<bool>,
    pub admin: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoState {
    pub archived: bool,
    pub disabled: bool,
    pub private: Option<bool>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSummary {
    pub path: String,
    pub name: Option<String>,
    /// Would a push to the branch start it?
    pub push: Tri,
    /// Would a pull request into the default branch start it?
    pub pull_request: Tri,
    pub dispatch: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightReport {
    /// False for a remote that is not on GitHub.
    pub supported: bool,
    pub reason: Option<String>,
    pub repository: Option<String>,
    /// The branch the triggers were checked against.
    pub branch: String,
    /// No blockers.
    pub ok: bool,
    /// One line: `ready`, `ready with warnings: ...` or `blocked: ...`.
    pub verdict: String,
    pub blockers: Vec<Issue>,
    pub warnings: Vec<Issue>,
    pub account: Account,
    pub permission: Permission,
    pub repo_state: RepoState,
    pub workflows: Vec<WorkflowSummary>,
    pub checked_at_ms: i64,
}

impl PreflightReport {
    /// A remote this check does not cover.
    pub fn unsupported(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            supported: false,
            verdict: format!("not checked: {reason}"),
            reason: Some(reason),
            ok: true,
            checked_at_ms: crate::now_ms(),
            ..Self::default()
        }
    }

    fn finish(&mut self) {
        self.ok = self.blockers.is_empty();
        let list = |issues: &[Issue]| {
            issues
                .iter()
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        };
        self.verdict = if !self.blockers.is_empty() {
            format!("blocked: {}", list(&self.blockers))
        } else if !self.warnings.is_empty() {
            format!("ready with warnings: {}", list(&self.warnings))
        } else {
            "ready".to_owned()
        };
    }
}

// ───────────────────────────── the checks

/// Run the checks for a checkout of `repository` (`owner/name`) that a writer
/// will push `branch` from.
pub(crate) async fn run_checks(
    runner: &dyn ProcessRunner,
    checkout: &Path,
    repository: &str,
    branch: &str,
) -> PreflightReport {
    if !valid_repository(repository) {
        return PreflightReport::unsupported(format!("{repository:?} is not a GitHub repository"));
    }
    let mut report = PreflightReport {
        supported: true,
        repository: Some(repository.to_owned()),
        branch: branch.to_owned(),
        checked_at_ms: crate::now_ms(),
        ..PreflightReport::default()
    };
    let repo_path = format!("repos/{repository}");
    let actions_path = format!("repos/{repository}/actions/permissions");
    let (user, repo, push, actions) = tokio::join!(
        account_of(runner),
        api(runner, &repo_path),
        git_push_probe(runner, checkout),
        api(runner, &actions_path),
    );

    // The gh account and its token.
    let mut gh_unusable = false;
    match user {
        Ok(account) => report.account = account,
        Err(Api::GhMissing) => {
            gh_unusable = true;
            report.warnings.push(issue(
                "ghMissing",
                "the GitHub CLI (gh) is not installed here, so CI cannot be read: install it and run `gh auth login`",
            ));
        }
        Err(Api::NotSignedIn) => {
            gh_unusable = true;
            report.warnings.push(issue(
                "ghNotSignedIn",
                "gh is not signed in, so CI cannot be read: run `gh auth login`",
            ));
        }
        Err(other) => {
            report.warnings.push(issue(
                "ghUserUnknown",
                format!("could not read the gh account: {}", other.describe()),
            ));
        }
    }

    // The repository.
    match repo {
        Ok(value) => {
            report.permission.gh_push = value["permissions"]["push"].as_bool();
            report.permission.admin = value["permissions"]["admin"].as_bool();
            report.repo_state = RepoState {
                archived: value["archived"].as_bool().unwrap_or(false),
                disabled: value["disabled"].as_bool().unwrap_or(false),
                private: value["private"].as_bool(),
                default_branch: value["default_branch"].as_str().map(str::to_owned),
            };
            if report.repo_state.archived {
                report
                    .blockers
                    .push(issue("archived", format!("{repository} is archived: nothing can be pushed")));
            }
            if report.repo_state.disabled {
                report
                    .blockers
                    .push(issue("disabled", format!("{repository} is disabled on GitHub")));
            }
        }
        Err(Api::Missing) if !gh_unusable => report.blockers.push(issue(
            "repoNotFound",
            format!(
                "GitHub shows no repository {repository} to this account: a private repository without access, or a wrong remote"
            ),
        )),
        Err(Api::Denied(message)) if message.to_lowercase().contains("saml") => {
            report.blockers.push(issue(
                "ssoRequired",
                format!(
                    "the token is not authorised for {repository}'s organisation (SAML SSO): authorise it on GitHub"
                ),
            ));
        }
        Err(Api::GhMissing | Api::NotSignedIn) => {}
        Err(other) => report.warnings.push(issue(
            "repoUnknown",
            format!("could not read {repository} from GitHub: {}", other.describe()),
        )),
    }

    // Scopes (a classic token only; a fine-grained one is judged by the
    // repository permissions above).
    if report.account.token_kind == "classic" {
        let has = |scope: &str| report.account.scopes.iter().any(|s| s == scope);
        let public = report.repo_state.private == Some(false);
        if !has("repo") && !(public && has("public_repo")) {
            report.blockers.push(issue(
                "scopeRepo",
                "the gh token lacks the `repo` scope, so pull requests and CI cannot be read: run `gh auth refresh -s repo`",
            ));
        }
    }

    // Whether the push credential git really uses can push.
    match (push, report.permission.gh_push) {
        (Push::Allowed, gh_push) => {
            report.permission.git_push = Some(true);
            if gh_push == Some(false) {
                report.warnings.push(issue(
                    "ghNoPush",
                    "git can push, but the gh login has no push access here, so opening a pull request with gh will fail",
                ));
            }
        }
        (Push::Denied(message), gh_push) => {
            report.permission.git_push = Some(false);
            let sso = message.to_lowercase().contains("saml");
            report.blockers.push(if sso {
                issue(
                    "ssoRequired",
                    "git's credentials are not authorised for this organisation (SAML SSO): authorise them on GitHub",
                )
            } else if gh_push == Some(true) {
                issue(
                    "gitCannotPush",
                    "git cannot push to the repository although the gh login can: git and gh use different credentials, and a writer pushes with git's. Fix git's credential helper or SSH key",
                )
            } else {
                issue(
                    "noPushAccess",
                    format!("the account has no push access to {repository}"),
                )
            });
        }
        (Push::Unknown(why), Some(false)) => {
            let _ = why;
            report.blockers.push(issue(
                "noPushAccess",
                format!("the account has no push access to {repository}"),
            ));
        }
        (Push::Unknown(why), _) => report.warnings.push(issue(
            "pushNotProbed",
            format!("could not check that git can push: {why}"),
        )),
    }

    // Actions.
    // (Reading the setting needs admin on some repositories: then it is
    // unknown, not a failure.)
    if let Ok(value) = actions
        && value["enabled"].as_bool() == Some(false)
    {
        report.blockers.push(issue(
            "actionsDisabled",
            format!("GitHub Actions is disabled for {repository}: no CI will run"),
        ));
    }

    // The workflows of the checkout and their triggers.
    let default_branch = report
        .repo_state
        .default_branch
        .clone()
        .unwrap_or_else(|| "main".to_owned());
    let root = checkout.to_path_buf();
    let workflows = tokio::task::spawn_blocking(move || scan_workflows(&root))
        .await
        .unwrap_or_default();
    report.workflows = workflows
        .iter()
        .map(|(path, facts)| WorkflowSummary {
            path: path.clone(),
            name: facts.name.clone(),
            push: would_run(&facts.push, branch, None),
            pull_request: would_run(&facts.pull_request, &default_branch, None),
            dispatch: facts.dispatch,
        })
        .collect();
    trigger_issues(&mut report, &workflows, branch);
    if !workflows.is_empty()
        && report.account.token_kind == "classic"
        && !report.account.scopes.iter().any(|s| s == "workflow")
    {
        report.warnings.push(issue(
            "scopeWorkflow",
            "the gh token lacks the `workflow` scope: a change to .github/workflows cannot be pushed with it (`gh auth refresh -s workflow`)",
        ));
    }

    // Self-hosted runners, and a billing block, only when gh can answer.
    if !gh_unusable {
        let wanted = self_hosted_labels(&workflows);
        let runners_path = format!("repos/{repository}/actions/runners");
        let (runners, billing) = tokio::join!(
            async {
                if wanted.is_empty() {
                    None
                } else {
                    Some(api(runner, &runners_path).await)
                }
            },
            billing_check(runner, repository, &default_branch),
        );
        if let Some(runners) = runners {
            runner_issues(&mut report, runners, &wanted);
        }
        match billing {
            Billing::Clear => {}
            Billing::Suspected(why) => report.warnings.push(issue("billingSuspected", why)),
            Billing::Blocked(why) => report.blockers.push(issue("billingBlocked", why)),
        }
    }

    report.finish();
    report
}

fn valid_repository(repository: &str) -> bool {
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    repository
        .split_once('/')
        .is_some_and(|(owner, name)| valid(owner) && valid(name))
}

fn gh(args: &[&str]) -> ProcessRequest {
    let mut request = github_request();
    request.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    request.timeout = CALL_TIMEOUT;
    request.output_limit = OUTPUT_LIMIT;
    request
}

/// Why an API call gave no data.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Api {
    GhMissing,
    NotSignedIn,
    /// 403: no permission (or SAML).
    Denied(String),
    /// 404.
    Missing,
    Failed(String),
}

impl Api {
    fn describe(&self) -> String {
        match self {
            Api::GhMissing => "gh is not installed".into(),
            Api::NotSignedIn => "gh is not signed in".into(),
            Api::Denied(message) | Api::Failed(message) => message.clone(),
            Api::Missing => "not found".into(),
        }
    }
}

/// Map a failed `gh` run to what it means.
fn classify_failure(output: &ProcessOutput) -> Api {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let lower = text.to_lowercase();
    let first: String = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("gh failed")
        .chars()
        .take(160)
        .collect();
    if lower.contains("rate limit") {
        Api::Failed("GitHub's rate limit was reached".into())
    } else if lower.contains("gh auth login")
        || lower.contains("not logged")
        || lower.contains("bad credentials")
        || lower.contains("http 401")
    {
        Api::NotSignedIn
    } else if lower.contains("http 403") || lower.contains("resource not accessible") {
        Api::Denied(first)
    } else if lower.contains("http 404") || lower.contains("not found") {
        Api::Missing
    } else {
        Api::Failed(first)
    }
}

fn classify_run_error(error: ProcessRunError) -> Api {
    match error {
        ProcessRunError::Spawn(std::io::ErrorKind::NotFound) => Api::GhMissing,
        ProcessRunError::Timeout => Api::Failed("timed out".into()),
        ProcessRunError::Spawn(_) | ProcessRunError::Io => Api::Failed("could not run gh".into()),
    }
}

/// `gh api <path>`: its JSON body.
async fn api(runner: &dyn ProcessRunner, path: &str) -> Result<Value, Api> {
    let output = runner
        .run(gh(&["api", path]))
        .await
        .map_err(classify_run_error)?;
    if !output.success {
        return Err(classify_failure(&output));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| Api::Failed("unreadable reply".into()))
}

/// `gh api -i user`: the login, and a classic token's scopes from the
/// `X-OAuth-Scopes` header (a fine-grained token sends none).
async fn account_of(runner: &dyn ProcessRunner) -> Result<Account, Api> {
    let output = runner
        .run(gh(&["api", "-i", "user"]))
        .await
        .map_err(classify_run_error)?;
    if !output.success {
        return Err(classify_failure(&output));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .unwrap_or(("", text.as_str()));
    let scopes = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("x-oauth-scopes").then(|| {
            value
                .split(',')
                .map(|scope| scope.trim().to_owned())
                .filter(|scope| !scope.is_empty())
                .collect::<Vec<_>>()
        })
    });
    let login = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value["login"].as_str().map(str::to_owned));
    Ok(Account {
        login,
        token_kind: if scopes.is_some() {
            "classic"
        } else {
            "fineGrained"
        }
        .to_owned(),
        scopes: scopes.unwrap_or_default(),
    })
}

/// What the push probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Push {
    Allowed,
    /// The remote refused git's credentials.
    Denied(String),
    /// The probe could not decide, and why.
    Unknown(String),
}

/// `git push --dry-run` from the checkout: GitHub answers the receive-pack
/// discovery with a refusal for a login that cannot write, and nothing is
/// created. Never prompts.
async fn git_push_probe(runner: &dyn ProcessRunner, checkout: &Path) -> Push {
    let request = ProcessRequest {
        program: "git".into(),
        args: vec![
            "push".into(),
            "--dry-run".into(),
            "origin".into(),
            format!("HEAD:{PROBE_REF}"),
        ],
        stdin: None,
        cwd: Some(checkout.to_path_buf()),
        env: vec![
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GCM_INTERACTIVE".into(), "never".into()),
        ],
        timeout: PUSH_PROBE_TIMEOUT,
        output_limit: OUTPUT_LIMIT,
    };
    let output = match runner.run(request).await {
        Ok(output) => output,
        Err(ProcessRunError::Spawn(std::io::ErrorKind::NotFound)) => {
            return Push::Unknown("git is not installed".into());
        }
        Err(ProcessRunError::Timeout) => {
            return Push::Unknown("the remote did not answer in time".into());
        }
        Err(_) => return Push::Unknown("could not run git".into()),
    };
    if output.success {
        return Push::Allowed;
    }
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    let lower = text.to_lowercase();
    let refused = [
        "permission denied",
        "403",
        "authentication failed",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "denied to",
        "access denied",
        "invalid username or password",
        "repository not found",
        "publickey",
        "saml",
    ];
    if refused.iter().any(|needle| lower.contains(needle)) {
        return Push::Denied(text.chars().take(300).collect());
    }
    Push::Unknown(
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("git push failed")
            .chars()
            .take(160)
            .collect(),
    )
}

/// What the workflows' triggers mean for this branch.
fn trigger_issues(
    report: &mut PreflightReport,
    workflows: &[(String, WorkflowFacts)],
    branch: &str,
) {
    if workflows.is_empty() {
        report.warnings.push(issue(
            "noWorkflows",
            "the repository has no workflows in .github/workflows, so no CI will run",
        ));
        return;
    }
    let push_yes = report.workflows.iter().any(|w| w.push == Tri::Yes);
    let push_unknown = report.workflows.iter().any(|w| w.push == Tri::Unknown);
    let pr_possible = report
        .workflows
        .iter()
        .any(|w| matches!(w.pull_request, Tri::Yes | Tri::Unknown));
    if !push_yes && !push_unknown {
        report.warnings.push(if pr_possible {
            issue(
                "ciStartsAtPullRequest",
                format!(
                    "pushing {branch} starts no CI; CI starts when a pull request is opened, so open a draft pull request right after the first push"
                ),
            )
        } else {
            issue(
                "noCi",
                format!("no workflow runs for a push to {branch} or for a pull request"),
            )
        });
    } else if !push_yes {
        report.warnings.push(issue(
            "ciUnclear",
            format!("could not tell whether a push to {branch} starts CI (a workflow's triggers are too dynamic to read)"),
        ));
    }
    // A pull request that touches none of the paths a workflow watches
    // starts nothing.
    let pr_workflows: Vec<&WorkflowFacts> = workflows
        .iter()
        .map(|(_, facts)| facts)
        .filter(|facts| matches!(facts.pull_request, Trigger::On(_)))
        .collect();
    if !pr_workflows.is_empty()
        && pr_workflows.iter().all(|facts| {
            matches!(&facts.pull_request, Trigger::On(f) if f.paths.is_some() || f.paths_ignore.is_some())
        })
        && !push_yes
    {
        report.warnings.push(issue(
            "pathFiltered",
            "every pull_request workflow has a paths filter: a change that touches none of those paths starts no CI (a docs-only pull request, for one)",
        ));
    }
}

/// The label sets of self-hosted runners the workflows ask for, with the
/// first workflow that does.
fn self_hosted_labels(workflows: &[(String, WorkflowFacts)]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (path, facts) in workflows {
        for labels in &facts.runs_on {
            if labels.iter().any(|label| label == "self-hosted")
                && !out.iter().any(|(_, seen)| seen == labels)
            {
                out.push((path.clone(), labels.clone()));
            }
        }
    }
    out
}

fn runner_issues(
    report: &mut PreflightReport,
    runners: Result<Value, Api>,
    wanted: &[(String, Vec<String>)],
) {
    let labels = |sets: &[(String, Vec<String>)]| {
        sets.iter()
            .map(|(_, labels)| labels.join(", "))
            .collect::<Vec<_>>()
            .join(" / ")
    };
    match runners {
        Ok(value) => {
            let online: Vec<Vec<String>> = value["runners"]
                .as_array()
                .map(|runners| {
                    runners
                        .iter()
                        .filter(|runner| runner["status"].as_str() == Some("online"))
                        .map(|runner| {
                            runner["labels"]
                                .as_array()
                                .map(|labels| {
                                    labels
                                        .iter()
                                        .filter_map(|label| label["name"].as_str())
                                        .map(str::to_lowercase)
                                        .collect()
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (path, needed) in wanted {
                if !online
                    .iter()
                    .any(|have| needed.iter().all(|label| have.contains(label)))
                {
                    report.warnings.push(issue(
                        "noOnlineRunner",
                        format!(
                            "{path} runs on a self-hosted runner ({}) and the repository lists none online; an organisation's runners are not listed here, so check them if CI hangs queued",
                            needed.join(", ")
                        ),
                    ));
                }
            }
        }
        Err(Api::Denied(_)) => report.warnings.push(issue(
            "runnersUnknown",
            format!(
                "workflows need self-hosted runners ({}), and listing runners needs admin access: could not check them",
                labels(wanted)
            ),
        )),
        Err(_) => {}
    }
}

/// A suspected block on Actions billing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Billing {
    Clear,
    Suspected(String),
    Blocked(String),
}

/// The default branch's last runs, when every one of them failed without
/// running a step: a failed payment or a spending limit does that, and the
/// annotation GitHub puts on the job says which. Only the annotation is
/// proof; the shape alone is a suspicion.
async fn billing_check(runner: &dyn ProcessRunner, repository: &str, branch: &str) -> Billing {
    let Ok(runs) = api(
        runner,
        &format!("repos/{repository}/actions/runs?per_page={BILLING_RUNS}&branch={branch}"),
    )
    .await
    else {
        return Billing::Clear;
    };
    let ids: Vec<u64> = runs["workflow_runs"]
        .as_array()
        .map(|runs| {
            runs.iter()
                .take(BILLING_RUNS)
                .filter(|run| run["conclusion"].as_str() == Some("failure"))
                .filter_map(|run| run["id"].as_u64())
                .collect()
        })
        .unwrap_or_default();
    let latest = runs["workflow_runs"]
        .as_array()
        .map_or(0, |runs| runs.len().min(BILLING_RUNS));
    // One that passed, or none at all, is no block.
    if latest == 0 || ids.len() != latest {
        return Billing::Clear;
    }
    let verdicts = futures::future::join_all(ids.iter().map(|id| async move {
        let Ok(jobs) = api(
            runner,
            &format!("repos/{repository}/actions/runs/{id}/jobs"),
        )
        .await
        else {
            return (false, false);
        };
        let jobs = jobs["jobs"].as_array().cloned().unwrap_or_default();
        let empty: Vec<&Value> = jobs
            .iter()
            .filter(|job| {
                job["conclusion"].as_str() == Some("failure")
                    && job["steps"].as_array().is_none_or(Vec::is_empty)
            })
            .collect();
        let Some(job) = empty.first().and_then(|job| job["id"].as_u64()) else {
            return (false, false);
        };
        let proven = match api(
            runner,
            &format!("repos/{repository}/check-runs/{job}/annotations"),
        )
        .await
        {
            Ok(annotations) => annotations.as_array().is_some_and(|list| {
                list.iter().any(|annotation| {
                    let message = annotation["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_lowercase();
                    message.contains("payments have failed") || message.contains("spending limit")
                })
            }),
            Err(_) => false,
        };
        (proven, true)
    }))
    .await;
    let proven = verdicts.iter().filter(|(proven, _)| *proven).count();
    let shaped = verdicts.iter().filter(|(_, shaped)| *shaped).count();
    let total = verdicts.len();
    if proven == total && total >= BILLING_RUNS {
        Billing::Blocked(format!(
            "GitHub Actions is blocked on billing for {repository} (a failed payment or the spending limit): the last {total} runs of {branch} failed before running a step, so a red CI there is not a code failure. Fix the billing, or expect no CI"
        ))
    } else if proven >= 1 {
        Billing::Suspected(format!(
            "{proven} of the last {total} runs of {branch} failed on GitHub's billing check (a failed payment or the spending limit); a red CI may not be a code failure"
        ))
    } else if shaped == total && total >= 1 {
        Billing::Suspected(format!(
            "the last {total} runs of {branch} failed without running a step, which looks like a billing block on Actions; a red CI may not be a code failure"
        ))
    } else {
        Billing::Clear
    }
}

// ───────────────────────────── the engine's handle

/// The preflight of one engine: the process runner, and reports kept for a
/// few minutes per repository and branch prefix.
pub struct GithubPreflight {
    runner: Arc<dyn ProcessRunner>,
    cache: Mutex<HashMap<String, (Instant, PreflightReport)>>,
}

impl Default for GithubPreflight {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubPreflight {
    pub fn new() -> Self {
        Self::with_runner(Arc::new(SystemProcessRunner))
    }

    pub(crate) fn with_runner(runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            runner,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The report for `checkout`, of `repository`, for a writer that will push
    /// `branch` (a `zeron/<name>` branch when none is named). `force` skips
    /// the cache.
    pub async fn check(
        &self,
        checkout: &Path,
        repository: &str,
        branch: Option<&str>,
        force: bool,
    ) -> PreflightReport {
        let branch = branch
            .map(str::trim)
            .filter(|branch| !branch.is_empty())
            .unwrap_or(DEFAULT_BRANCH);
        // Branches of one prefix (zeron/*) trigger the same workflows.
        let prefix = branch.split_once('/').map_or(branch, |(prefix, _)| prefix);
        let key = format!("{}|{prefix}", repository.to_lowercase());
        if !force
            && let Some((at, report)) = self
                .cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&key)
            && at.elapsed() < CACHE_TTL
        {
            return report.clone();
        }
        let report = run_checks(self.runner.as_ref(), checkout, repository, branch).await;
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, (Instant::now(), report.clone()));
        report
    }
}

// ───────────────────────────── workflow triggers

/// The filters of one event in a workflow's `on:`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EventFilter {
    branches: Option<Vec<String>>,
    branches_ignore: Option<Vec<String>>,
    /// `tags` or `tags-ignore`: a filter on tags alone never matches a branch.
    tags: Option<Vec<String>>,
    paths: Option<Vec<String>>,
    paths_ignore: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// The workflow does not listen for the event.
    Absent,
    On(EventFilter),
    /// Written in a way the scanner does not read.
    Unknown,
}

/// What the scanner reads of a workflow file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowFacts {
    pub(crate) name: Option<String>,
    pub(crate) push: Trigger,
    pub(crate) pull_request: Trigger,
    pub(crate) dispatch: bool,
    /// The literal `runs-on` label sets, lower case. An expression or a group
    /// is left out: it names no label to look for.
    pub(crate) runs_on: Vec<Vec<String>>,
}

/// The workflow files of a checkout, in name order.
fn scan_workflows(checkout: &Path) -> Vec<(String, WorkflowFacts)> {
    let dir: PathBuf = checkout.join(".github").join("workflows");
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml")
                })
        })
        .collect();
    files.sort();
    files.truncate(MAX_WORKFLOW_FILES);
    files
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let text = std::fs::metadata(&path)
                .ok()
                .filter(|meta| meta.len() <= MAX_WORKFLOW_BYTES)
                .and_then(|_| std::fs::read_to_string(&path).ok());
            let facts = match text {
                Some(text) => parse_workflow(&text),
                None => WorkflowFacts {
                    name: None,
                    push: Trigger::Unknown,
                    pull_request: Trigger::Unknown,
                    dispatch: false,
                    runs_on: Vec::new(),
                },
            };
            (format!(".github/workflows/{name}"), facts)
        })
        .collect()
}

struct Line {
    indent: usize,
    text: String,
}

/// `text` without its `#` comment (a `#` inside quotes stays).
fn strip_comment(text: &str) -> String {
    let (mut single, mut double) = (false, false);
    let mut previous = ' ';
    for (at, c) in text.char_indices() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '#' if !single && !double && previous.is_whitespace() => {
                return text[..at].trim_end().to_owned();
            }
            _ => {}
        }
        previous = c;
    }
    text.trim_end().to_owned()
}

fn unquote(text: &str) -> String {
    let text = text.trim();
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            return text[1..text.len() - 1].to_owned();
        }
    }
    text.to_owned()
}

/// `key: rest` of a line (`None` for a list item or a line without a colon).
fn key_value(text: &str) -> Option<(String, String)> {
    if text.starts_with("- ") || text == "-" {
        return None;
    }
    let colon = text.find(':')?;
    let key = unquote(&text[..colon]);
    Some((key, text[colon + 1..].trim().to_owned()))
}

/// `[a, 'b']` as its items; `None` when it holds anything the scanner does
/// not read.
fn flow_list(text: &str) -> Option<Vec<String>> {
    let inner = text.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut items = Vec::new();
    for piece in inner.split(',') {
        let item = unquote(piece);
        if item.is_empty() {
            continue;
        }
        if item.contains(['{', '[', '$']) {
            return None;
        }
        items.push(item);
    }
    Some(items)
}

/// The value list of a `branches:`-style key: inline, or the block list after
/// it. Returns the list and how many following lines it used.
fn filter_values(rest: &str, after: &[Line], key_indent: usize) -> Option<(Vec<String>, usize)> {
    if rest.contains("${{") {
        return None;
    }
    if rest.starts_with('[') {
        return flow_list(rest).map(|list| (list, 0));
    }
    if !rest.is_empty() {
        return Some((vec![unquote(rest)], 0));
    }
    let mut items = Vec::new();
    let mut used = 0;
    for line in after {
        if line.indent < key_indent {
            break;
        }
        match line.text.strip_prefix("- ") {
            Some(item) => {
                if item.contains("${{") {
                    return None;
                }
                items.push(unquote(item));
                used += 1;
            }
            None if line.text == "-" => return None,
            None if line.indent > key_indent => return None,
            None => break,
        }
    }
    Some((items, used))
}

/// The filters under an event key (`push:`), or `None` when they are not
/// readable.
fn parse_filter(lines: &[Line]) -> Option<EventFilter> {
    let mut filter = EventFilter::default();
    let Some(first) = lines.first() else {
        return Some(filter);
    };
    let indent = first.indent;
    let mut at = 0;
    while at < lines.len() {
        let line = &lines[at];
        at += 1;
        if line.indent != indent {
            continue;
        }
        let Some((key, rest)) = key_value(&line.text) else {
            continue;
        };
        let slot = match key.as_str() {
            "branches" => &mut filter.branches,
            "branches-ignore" => &mut filter.branches_ignore,
            "tags" | "tags-ignore" => &mut filter.tags,
            "paths" => &mut filter.paths,
            "paths-ignore" => &mut filter.paths_ignore,
            _ => continue,
        };
        let (values, used) = filter_values(&rest, &lines[at..], indent)?;
        *slot = Some(values);
        at += used;
    }
    Some(filter)
}

/// Read what matters of a workflow file: the events it listens for and where
/// it runs. Written for the shapes workflows use; anything else reads as
/// unknown, never as a trigger that will fire.
pub(crate) fn parse_workflow(text: &str) -> WorkflowFacts {
    let lines: Vec<Line> = text
        .lines()
        .map(strip_comment)
        .filter(|line| !line.trim().is_empty() && line.trim() != "---")
        .map(|line| {
            let indent = line.len() - line.trim_start_matches(' ').len();
            Line {
                indent,
                text: line.trim().to_owned(),
            }
        })
        .collect();
    let mut facts = WorkflowFacts {
        name: None,
        push: Trigger::Absent,
        pull_request: Trigger::Absent,
        dispatch: false,
        runs_on: Vec::new(),
    };
    facts.name = lines
        .iter()
        .filter(|line| line.indent == 0)
        .find_map(|line| key_value(&line.text).filter(|(key, _)| key == "name"))
        .map(|(_, value)| unquote(&value))
        .filter(|name| !name.is_empty());

    // `on:` (YAML 1.1 also reads a bare `on` key as `true`).
    let on = lines.iter().position(|line| {
        line.indent == 0
            && key_value(&line.text).is_some_and(|(key, _)| key == "on" || key == "true")
    });
    let Some(on) = on else {
        facts.push = Trigger::Unknown;
        facts.pull_request = Trigger::Unknown;
        read_runs_on(&lines, &mut facts);
        return facts;
    };
    let (_, rest) = key_value(&lines[on].text).unwrap_or_default();
    let set = |event: &str, trigger: Trigger, facts: &mut WorkflowFacts| match event {
        "push" => facts.push = trigger,
        "pull_request" => facts.pull_request = trigger,
        "workflow_dispatch" => facts.dispatch = true,
        _ => {}
    };
    if rest.starts_with('{') {
        facts.push = Trigger::Unknown;
        facts.pull_request = Trigger::Unknown;
    } else if rest.starts_with('[') {
        match flow_list(&rest) {
            Some(events) => {
                for event in events {
                    set(&event, Trigger::On(EventFilter::default()), &mut facts);
                }
            }
            None => {
                facts.push = Trigger::Unknown;
                facts.pull_request = Trigger::Unknown;
            }
        }
    } else if !rest.is_empty() {
        set(
            &unquote(&rest),
            Trigger::On(EventFilter::default()),
            &mut facts,
        );
    } else {
        let block: Vec<&Line> = lines[on + 1..]
            .iter()
            .take_while(|line| line.indent > 0)
            .collect();
        if let Some(event_indent) = block.first().map(|line| line.indent) {
            let mut at = 0;
            while at < block.len() {
                let line = block[at];
                at += 1;
                if line.indent != event_indent {
                    continue;
                }
                if let Some(event) = line.text.strip_prefix("- ") {
                    set(
                        &unquote(event),
                        Trigger::On(EventFilter::default()),
                        &mut facts,
                    );
                    continue;
                }
                let Some((event, rest)) = key_value(&line.text) else {
                    continue;
                };
                let mut sub = Vec::new();
                while at < block.len() && block[at].indent > event_indent {
                    sub.push(Line {
                        indent: block[at].indent,
                        text: block[at].text.clone(),
                    });
                    at += 1;
                }
                let trigger = if rest.starts_with('{') || rest.starts_with('[') {
                    Trigger::Unknown
                } else {
                    match parse_filter(&sub) {
                        Some(filter) => Trigger::On(filter),
                        None => Trigger::Unknown,
                    }
                };
                set(&event, trigger, &mut facts);
            }
        }
    }
    read_runs_on(&lines, &mut facts);
    facts
}

fn read_runs_on(lines: &[Line], facts: &mut WorkflowFacts) {
    for (at, line) in lines.iter().enumerate() {
        let Some((key, rest)) = key_value(&line.text) else {
            continue;
        };
        if key != "runs-on" {
            continue;
        }
        if rest.contains("${{") {
            continue;
        } else if rest.starts_with('[') {
            if let Some(labels) = flow_list(&rest) {
                facts
                    .runs_on
                    .push(labels.iter().map(|label| label.to_lowercase()).collect());
            }
        } else if rest.is_empty() {
            let mut labels = Vec::new();
            for next in &lines[at + 1..] {
                match next.text.strip_prefix("- ") {
                    Some(label) if next.indent >= line.indent => {
                        labels.push(unquote(label).to_lowercase())
                    }
                    _ => break,
                }
            }
            // A `group:` / `labels:` mapping is not a list of labels.
            if !labels.is_empty() {
                facts.runs_on.push(labels);
            }
        } else {
            facts.runs_on.push(vec![unquote(&rest).to_lowercase()]);
        }
    }
}

/// Would an event with this trigger start the workflow for `branch` (and,
/// when known, a change to `changed` paths)?
pub(crate) fn would_run(trigger: &Trigger, branch: &str, changed: Option<&[String]>) -> Tri {
    let filter = match trigger {
        Trigger::Absent => return Tri::No,
        Trigger::Unknown => return Tri::Unknown,
        Trigger::On(filter) => filter,
    };
    if filter.branches.is_none() && filter.branches_ignore.is_none() && filter.tags.is_some() {
        return Tri::No;
    }
    if let Some(patterns) = &filter.branches {
        match matches_patterns(patterns, branch) {
            Some(true) => {}
            Some(false) => return Tri::No,
            None => return Tri::Unknown,
        }
    }
    if let Some(patterns) = &filter.branches_ignore {
        match matches_patterns(patterns, branch) {
            Some(true) => return Tri::No,
            Some(false) => {}
            None => return Tri::Unknown,
        }
    }
    if filter.paths.is_some() || filter.paths_ignore.is_some() {
        let Some(changed) = changed else {
            return Tri::Unknown;
        };
        if let Some(patterns) = &filter.paths {
            let mut any = false;
            for path in changed {
                match matches_patterns(patterns, path) {
                    Some(true) => any = true,
                    Some(false) => {}
                    None => return Tri::Unknown,
                }
            }
            if !any {
                return Tri::No;
            }
        }
        if let Some(patterns) = &filter.paths_ignore {
            let mut any_kept = false;
            for path in changed {
                match matches_patterns(patterns, path) {
                    Some(false) => any_kept = true,
                    Some(true) => {}
                    None => return Tri::Unknown,
                }
            }
            if !any_kept {
                return Tri::No;
            }
        }
    }
    Tri::Yes
}

/// A GitHub filter list against `text`: patterns in order, a `!` pattern
/// takes a match back. `None` for a pattern the matcher does not read.
fn matches_patterns(patterns: &[String], text: &str) -> Option<bool> {
    let mut matched = false;
    for pattern in patterns {
        let (negate, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern.as_str()),
        };
        if glob_matches(pattern, text)? {
            matched = !negate;
        }
    }
    Some(matched)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    Literal(char),
    /// `c?`: the character, or nothing.
    Optional(char),
    /// `c+`: the character, one or more times.
    Repeated(char),
    /// `*`: anything but `/`.
    Star,
    /// `**`: anything.
    Globstar,
}

/// GitHub's filter globs: `*` (not across `/`), `**`, `?` and `+` on the
/// preceding character. `None` for a character class, which is not read.
fn glob_matches(pattern: &str, text: &str) -> Option<bool> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                while chars.peek() == Some(&'*') {
                    chars.next();
                }
                pieces.push(Piece::Globstar);
            }
            '*' => pieces.push(Piece::Star),
            '?' | '+' => match pieces.pop() {
                Some(Piece::Literal(prev)) => pieces.push(if c == '?' {
                    Piece::Optional(prev)
                } else {
                    Piece::Repeated(prev)
                }),
                _ => return None,
            },
            '[' | ']' | '\\' => return None,
            c => pieces.push(Piece::Literal(c)),
        }
    }
    let text: Vec<char> = text.chars().collect();
    Some(piece_match(&pieces, &text))
}

fn piece_match(pieces: &[Piece], text: &[char]) -> bool {
    match pieces.split_first() {
        None => text.is_empty(),
        Some((Piece::Literal(c), rest)) => text.first() == Some(c) && piece_match(rest, &text[1..]),
        Some((Piece::Optional(c), rest)) => {
            (text.first() == Some(c) && piece_match(rest, &text[1..])) || piece_match(rest, text)
        }
        Some((Piece::Repeated(c), rest)) => {
            let run = text.iter().take_while(|t| *t == c).count();
            (1..=run).any(|used| piece_match(rest, &text[used..]))
        }
        Some((Piece::Star, rest)) => {
            let reach = text.iter().position(|t| *t == '/').unwrap_or(text.len());
            (0..=reach).any(|used| piece_match(rest, &text[used..]))
        }
        Some((Piece::Globstar, rest)) => {
            (0..=text.len()).any(|used| piece_match(rest, &text[used..]))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    // ── triggers, from this repository's own workflows ─────────────────────

    const CURSOR: &str = "\
name: Cursor compatibility
on:
  pull_request:
    paths:
      - 'crates/harness/**'
      - 'Cargo.toml'
      - '.github/workflows/cursor-*.yml'
  push:
    branches: [main]
    paths:
      - 'crates/harness/**'
      - 'Cargo.toml'
  workflow_dispatch:
jobs:
  check:
    runs-on: ubuntu-24.04
";

    const MACOS: &str = "\
name: macOS tests

on:
  pull_request:
    paths:
      - 'Cargo.toml'
      - 'crates/**'
      - '.github/workflows/macos.yml'

jobs:
  tests:
    runs-on: macos-latest # Apple silicon
  fmt:
    runs-on: ubuntu-24.04
";

    const DEPLOY: &str = "\
name: deploy

# Autodeploy the edge Worker on every push to main that touches edge/.

on:
  push:
    branches: [main]
    paths:
      - 'edge/**'

jobs:
  deploy:
    runs-on: ubuntu-latest
";

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn a_push_to_a_zeron_branch_starts_none_of_this_repositorys_workflows() {
        let cursor = parse_workflow(CURSOR);
        assert_eq!(cursor.name.as_deref(), Some("Cursor compatibility"));
        assert!(cursor.dispatch);
        assert_eq!(would_run(&cursor.push, "zeron/foo", None), Tri::No);
        assert_eq!(
            would_run(&cursor.push, "main", Some(&paths(&["Cargo.toml"]))),
            Tri::Yes
        );
        assert_eq!(
            would_run(&cursor.push, "main", Some(&paths(&["README.md"]))),
            Tri::No
        );
        assert_eq!(would_run(&cursor.push, "main", None), Tri::Unknown);
        // A pull request runs it only for a change under its paths.
        let pr = &cursor.pull_request;
        assert_eq!(
            would_run(pr, "main", Some(&paths(&["crates/harness/src/a.rs"]))),
            Tri::Yes
        );
        assert_eq!(would_run(pr, "main", Some(&paths(&["docs/a.md"]))), Tri::No);
        assert_eq!(would_run(pr, "main", None), Tri::Unknown);

        let macos = parse_workflow(MACOS);
        assert_eq!(would_run(&macos.push, "zeron/foo", None), Tri::No);
        assert_eq!(
            would_run(
                &macos.pull_request,
                "main",
                Some(&paths(&["crates/ui/x.rs"]))
            ),
            Tri::Yes
        );
        assert_eq!(macos.runs_on, [["macos-latest"], ["ubuntu-24.04"]]);

        let deploy = parse_workflow(DEPLOY);
        assert_eq!(would_run(&deploy.push, "zeron/foo", None), Tri::No);
        assert_eq!(would_run(&deploy.pull_request, "main", None), Tri::No);
    }

    #[test]
    fn the_short_and_flow_forms_of_on_are_read() {
        let bare = parse_workflow("on: push\njobs:\n  a:\n    runs-on: x\n");
        assert_eq!(would_run(&bare.push, "zeron/x", None), Tri::Yes);
        assert_eq!(would_run(&bare.pull_request, "main", None), Tri::No);
        let both = parse_workflow("on: [push, pull_request]\n");
        assert_eq!(would_run(&both.push, "zeron/x", None), Tri::Yes);
        assert_eq!(would_run(&both.pull_request, "main", None), Tri::Yes);
        let list = parse_workflow("on:\n  - push\n  - workflow_dispatch\n");
        assert_eq!(would_run(&list.push, "zeron/x", None), Tri::Yes);
        assert!(list.dispatch);
        let only_dispatch =
            parse_workflow("on:\n  workflow_dispatch:\n  schedule:\n    - cron: '0 0 * * *'\n");
        assert_eq!(would_run(&only_dispatch.push, "zeron/x", None), Tri::No);
        let quoted = parse_workflow("\"on\":\n  push:\n    branches:\n      - 'zeron/**'\n");
        assert_eq!(would_run(&quoted.push, "zeron/a/b", None), Tri::Yes);
        assert_eq!(would_run(&quoted.push, "main", None), Tri::No);
    }

    #[test]
    fn branch_filters_use_githubs_globs() {
        let ignore = parse_workflow("on:\n  push:\n    branches-ignore:\n      - 'zeron/**'\n");
        assert_eq!(would_run(&ignore.push, "zeron/foo", None), Tri::No);
        assert_eq!(would_run(&ignore.push, "feature", None), Tri::Yes);
        let tags = parse_workflow("on:\n  push:\n    tags:\n      - 'v*'\n");
        assert_eq!(would_run(&tags.push, "zeron/foo", None), Tri::No);
        let negated = parse_workflow(
            "on:\n  push:\n    branches:\n      - 'zeron/**'\n      - '!zeron/wip'\n",
        );
        assert_eq!(would_run(&negated.push, "zeron/foo", None), Tri::Yes);
        assert_eq!(would_run(&negated.push, "zeron/wip", None), Tri::No);
        let ignored = parse_workflow(
            "on:\n  pull_request:\n    paths-ignore:\n      - 'docs/**'\n      - '*.md'\n",
        );
        assert_eq!(
            would_run(
                &ignored.pull_request,
                "main",
                Some(&paths(&["docs/a/b.md"]))
            ),
            Tri::No
        );
        assert_eq!(
            would_run(
                &ignored.pull_request,
                "main",
                Some(&paths(&["docs/a.md", "src/x.rs"]))
            ),
            Tri::Yes
        );
        assert_eq!(glob_matches("zeron/*", "zeron/a"), Some(true));
        assert_eq!(
            glob_matches("zeron/*", "zeron/a/b"),
            Some(false),
            "* stops at /"
        );
        assert_eq!(glob_matches("zeron/**", "zeron/a/b"), Some(true));
        assert_eq!(glob_matches("releases/10?", "releases/10"), Some(true));
        assert_eq!(glob_matches("releases/10?", "releases/101"), Some(false));
        assert_eq!(glob_matches("v1+", "v111"), Some(true));
        assert_eq!(glob_matches("v1+", "v"), Some(false));
        assert_eq!(glob_matches("a[bc]", "ab"), None, "a class is not read");
    }

    #[test]
    fn what_the_scanner_cannot_read_is_unknown_never_a_trigger() {
        let flow = parse_workflow("on: { push: { branches: [main] } }\n");
        assert_eq!(would_run(&flow.push, "zeron/x", None), Tri::Unknown);
        let expression =
            parse_workflow("on:\n  push:\n    branches:\n      - ${{ vars.BRANCH }}\n");
        assert_eq!(would_run(&expression.push, "zeron/x", None), Tri::Unknown);
        let none = parse_workflow("jobs:\n  a:\n    runs-on: x\n");
        assert_eq!(would_run(&none.push, "zeron/x", None), Tri::Unknown);
        assert_eq!(would_run(&none.pull_request, "main", None), Tri::Unknown);
        let matrix = parse_workflow("on: push\njobs:\n  a:\n    runs-on: ${{ matrix.os }}\n");
        assert!(matrix.runs_on.is_empty(), "an expression names no label");
        let selfhosted = parse_workflow(
            "on: push\njobs:\n  a:\n    runs-on: [self-hosted, Linux]\n  b:\n    runs-on:\n      - self-hosted\n      - gpu\n",
        );
        assert_eq!(
            selfhosted.runs_on,
            [["self-hosted", "linux"], ["self-hosted", "gpu"]]
        );
        let group = parse_workflow("on: push\njobs:\n  a:\n    runs-on:\n      group: big\n");
        assert!(group.runs_on.is_empty());
    }

    // ── the checks, against a scripted gh and git ──────────────────────────

    /// Answers a command line with a scripted result: the rule whose text
    /// equals the joined command, else a 404.
    #[derive(Default)]
    struct Script {
        rules: Mutex<Vec<(String, Result<ProcessOutput, ProcessRunError>)>>,
        calls: Mutex<VecDeque<String>>,
    }

    impl Script {
        /// Answer `command` with `result`, in place of an earlier answer.
        fn rule(self: &Arc<Self>, command: &str, result: Result<ProcessOutput, ProcessRunError>) {
            let mut rules = self.rules.lock().unwrap();
            rules.retain(|(known, _)| known != command);
            rules.push((command.to_owned(), result));
        }

        fn calls(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl ProcessRunner for Script {
        async fn run(&self, request: ProcessRequest) -> Result<ProcessOutput, ProcessRunError> {
            let line = format!("{} {}", request.program, request.args.join(" "));
            self.calls.lock().unwrap().push_back(line.clone());
            self.rules
                .lock()
                .unwrap()
                .iter()
                .find(|(command, _)| *command == line)
                .map(|(_, result)| result.clone())
                .unwrap_or_else(|| Ok(fail("HTTP 404: Not Found")))
        }
    }

    fn done(stdout: &str) -> ProcessOutput {
        ProcessOutput {
            success: true,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
        }
    }

    fn fail(stderr: &str) -> ProcessOutput {
        ProcessOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_truncated: false,
        }
    }

    const PUSH: &str = "git push --dry-run origin HEAD:refs/heads/zeron/preflight-probe";

    fn user(scopes: Option<&str>) -> Result<ProcessOutput, ProcessRunError> {
        let head = match scopes {
            Some(scopes) => {
                format!("HTTP/2.0 200 OK\r\nX-Oauth-Scopes: {scopes}\r\nServer: GitHub.com\r\n\r\n")
            }
            None => "HTTP/2.0 200 OK\r\nServer: GitHub.com\r\n\r\n".to_owned(),
        };
        Ok(done(&format!("{head}{{\"login\":\"octo\"}}")))
    }

    fn repo(push: bool) -> Result<ProcessOutput, ProcessRunError> {
        Ok(done(&format!(
            "{{\"permissions\":{{\"admin\":false,\"push\":{push}}},\"archived\":false,\"disabled\":false,\"private\":false,\"default_branch\":\"main\"}}"
        )))
    }

    /// A healthy repository: classic token with every scope, push allowed
    /// both ways, Actions on, and a clean history.
    fn healthy() -> Arc<Script> {
        let script = Arc::new(Script::default());
        script.rule("gh api -i user", user(Some("repo, workflow")));
        script.rule("gh api repos/o/r", repo(true));
        script.rule(PUSH, Ok(done("")));
        script.rule(
            "gh api repos/o/r/actions/permissions",
            Ok(done("{\"enabled\":true}")),
        );
        script.rule(
            "gh api repos/o/r/actions/runs?per_page=3&branch=main",
            Ok(done(
                "{\"workflow_runs\":[{\"id\":1,\"conclusion\":\"success\"}]}",
            )),
        );
        script
    }

    fn checkout_with(workflows: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&root).unwrap();
        for (name, text) in workflows {
            std::fs::write(root.join(name), text).unwrap();
        }
        dir
    }

    async fn check(script: &Arc<Script>, dir: &tempfile::TempDir) -> PreflightReport {
        run_checks(&**script, dir.path(), "o/r", "zeron/x").await
    }

    fn codes(issues: &[Issue]) -> Vec<&'static str> {
        issues.iter().map(|issue| issue.code).collect()
    }

    #[tokio::test]
    async fn a_healthy_repository_is_ready_and_a_pull_request_only_ci_is_flagged() {
        let script = healthy();
        let dir = checkout_with(&[(
            "ci.yml",
            "on:\n  pull_request:\n  push:\n    branches: [main]\njobs:\n  a:\n    runs-on: ubuntu-24.04\n",
        )]);
        let report = check(&script, &dir).await;
        assert!(report.ok, "{report:?}");
        assert!(report.blockers.is_empty());
        assert_eq!(
            codes(&report.warnings),
            ["ciStartsAtPullRequest"],
            "{report:?}"
        );
        assert!(report.verdict.starts_with("ready with warnings"));
        assert_eq!(report.account.login.as_deref(), Some("octo"));
        assert_eq!(report.account.token_kind, "classic");
        assert_eq!(report.account.scopes, ["repo", "workflow"]);
        assert_eq!(report.permission.git_push, Some(true));
        assert_eq!(report.permission.gh_push, Some(true));
        assert_eq!(report.workflows[0].push, Tri::No);
        assert_eq!(report.workflows[0].pull_request, Tri::Yes);

        // A workflow that runs for the branch leaves nothing to say.
        let dir = checkout_with(&[("ci.yml", "on:\n  push:\n    branches: ['zeron/**']\n")]);
        let report = check(&script, &dir).await;
        assert_eq!(report.verdict, "ready", "{report:?}");
    }

    #[tokio::test]
    async fn a_login_without_push_access_blocks() {
        let script = healthy();
        script.rule("gh api repos/o/r", repo(false));
        script.rule(
            PUSH,
            Ok(fail("remote: Write access to repository not granted.\nfatal: unable to access: The requested URL returned error: 403")),
        );
        let report = check(&script, &checkout_with(&[])).await;
        assert!(!report.ok);
        assert_eq!(codes(&report.blockers), ["noPushAccess"], "{report:?}");
        assert!(report.verdict.starts_with("blocked"));
    }

    #[tokio::test]
    async fn gh_may_push_while_git_cannot() {
        let script = Arc::new(Script::default());
        script.rule("gh api -i user", user(Some("repo")));
        script.rule("gh api repos/o/r", repo(true));
        script.rule(PUSH, Ok(fail("fatal: could not read Username for 'https://github.com': terminal prompts disabled")));
        let report = check(&script, &checkout_with(&[])).await;
        assert_eq!(codes(&report.blockers), ["gitCannotPush"], "{report:?}");
        assert!(report.blockers[0].message.contains("different credentials"));
        assert_eq!(report.permission.git_push, Some(false));
    }

    #[tokio::test]
    async fn git_may_push_while_gh_cannot() {
        let script = healthy();
        script.rule("gh api repos/o/r", repo(false));
        let report = check(&script, &checkout_with(&[])).await;
        assert!(
            report.ok,
            "git's credential is the one a writer pushes with: {report:?}"
        );
        assert!(codes(&report.warnings).contains(&"ghNoPush"));
    }

    #[tokio::test]
    async fn an_unprobed_push_is_a_warning_not_a_pass() {
        let script = healthy();
        script.rule(
            PUSH,
            Ok(fail(
                "fatal: 'origin' does not appear to be a git repository",
            )),
        );
        let report = check(&script, &checkout_with(&[])).await;
        assert!(report.ok);
        assert!(
            codes(&report.warnings).contains(&"pushNotProbed"),
            "{report:?}"
        );
        assert_eq!(report.permission.git_push, None);
    }

    #[tokio::test]
    async fn scopes_come_from_the_header_and_a_fine_grained_token_has_none() {
        let script = healthy();
        script.rule("gh api -i user", user(Some("read:org")));
        let report = check(&script, &checkout_with(&[("a.yml", "on: push\n")])).await;
        assert_eq!(codes(&report.blockers), ["scopeRepo"], "{report:?}");
        assert!(codes(&report.warnings).contains(&"scopeWorkflow"));

        script.rule("gh api -i user", user(None));
        let report = check(&script, &checkout_with(&[("a.yml", "on: push\n")])).await;
        assert!(report.ok, "{report:?}");
        assert_eq!(report.account.token_kind, "fineGrained");
        assert!(report.account.scopes.is_empty());
        assert!(!codes(&report.warnings).contains(&"scopeWorkflow"));

        // A public repository takes public_repo in place of repo.
        let public = healthy();
        public.rule("gh api -i user", user(Some("public_repo")));
        assert!(check(&public, &checkout_with(&[])).await.ok);
    }

    #[tokio::test]
    async fn an_archived_repository_and_disabled_actions_block() {
        let script = Arc::new(Script::default());
        script.rule("gh api -i user", user(Some("repo")));
        script.rule(
            "gh api repos/o/r",
            Ok(done("{\"permissions\":{\"push\":true},\"archived\":true,\"disabled\":false,\"private\":false,\"default_branch\":\"main\"}")),
        );
        script.rule(PUSH, Ok(done("")));
        script.rule(
            "gh api repos/o/r/actions/permissions",
            Ok(done("{\"enabled\":false}")),
        );
        let report = check(&script, &checkout_with(&[])).await;
        assert_eq!(
            codes(&report.blockers),
            ["archived", "actionsDisabled"],
            "{report:?}"
        );
    }

    #[tokio::test]
    async fn a_403_on_actions_or_runners_is_unknown_not_a_failure() {
        let script = healthy();
        script.rule(
            "gh api repos/o/r/actions/permissions",
            Ok(fail("HTTP 403: Must have admin rights")),
        );
        script.rule(
            "gh api repos/o/r/actions/runners",
            Ok(fail("HTTP 403: Must have admin rights")),
        );
        let dir = checkout_with(&[(
            "a.yml",
            "on: push\njobs:\n  a:\n    runs-on: [self-hosted, gpu]\n",
        )]);
        let report = check(&script, &dir).await;
        assert!(report.ok, "{report:?}");
        assert_eq!(codes(&report.warnings), ["runnersUnknown"], "{report:?}");
    }

    #[tokio::test]
    async fn a_self_hosted_label_with_no_online_runner_is_flagged_and_an_expression_is_not() {
        let script = healthy();
        script.rule(
            "gh api repos/o/r/actions/runners",
            Ok(done("{\"runners\":[{\"status\":\"offline\",\"labels\":[{\"name\":\"self-hosted\"},{\"name\":\"gpu\"}]},{\"status\":\"online\",\"labels\":[{\"name\":\"self-hosted\"},{\"name\":\"Linux\"}]}]}")),
        );
        let dir = checkout_with(&[
            (
                "gpu.yml",
                "on: push\njobs:\n  a:\n    runs-on: [self-hosted, gpu]\n",
            ),
            (
                "linux.yml",
                "on: push\njobs:\n  a:\n    runs-on: [self-hosted, linux]\n",
            ),
            (
                "matrix.yml",
                "on: push\njobs:\n  a:\n    runs-on: ${{ matrix.os }}\n",
            ),
        ]);
        let report = check(&script, &dir).await;
        assert_eq!(codes(&report.warnings), ["noOnlineRunner"], "{report:?}");
        assert!(report.warnings[0].message.contains("gpu.yml"));
    }

    #[tokio::test]
    async fn a_missing_gh_is_a_warning_and_the_push_probe_still_decides() {
        let script = Arc::new(Script::default());
        script.rule(
            "gh api -i user",
            Err(ProcessRunError::Spawn(std::io::ErrorKind::NotFound)),
        );
        script.rule(
            "gh api repos/o/r",
            Err(ProcessRunError::Spawn(std::io::ErrorKind::NotFound)),
        );
        script.rule(
            "gh api repos/o/r/actions/permissions",
            Err(ProcessRunError::Spawn(std::io::ErrorKind::NotFound)),
        );
        script.rule(PUSH, Ok(done("")));
        let report = check(&script, &checkout_with(&[])).await;
        assert!(report.ok, "{report:?}");
        assert!(codes(&report.warnings).contains(&"ghMissing"));
        assert_eq!(report.permission.git_push, Some(true));
    }

    fn runs(conclusions: &[&str]) -> Result<ProcessOutput, ProcessRunError> {
        let runs: Vec<String> = conclusions
            .iter()
            .enumerate()
            .map(|(n, c)| format!("{{\"id\":{},\"conclusion\":\"{c}\"}}", n + 1))
            .collect();
        Ok(done(&format!("{{\"workflow_runs\":[{}]}}", runs.join(","))))
    }

    fn no_step_job(id: u64) -> Result<ProcessOutput, ProcessRunError> {
        Ok(done(&format!(
            "{{\"jobs\":[{{\"id\":{id},\"conclusion\":\"failure\",\"steps\":[]}}]}}"
        )))
    }

    #[tokio::test]
    async fn a_billing_block_is_proven_by_the_annotation_and_only_suspected_by_the_job_shape() {
        let list = "gh api repos/o/r/actions/runs?per_page=3&branch=main";
        let message = Ok(done(
            "[{\"message\":\"The job was not started because recent account payments have failed or your spending limit needs to be increased.\"}]",
        ));

        // Three runs, all proven: blocked.
        let script = healthy();
        script.rule(list, runs(&["failure", "failure", "failure"]));
        for id in 1..=3 {
            script.rule(
                &format!("gh api repos/o/r/actions/runs/{id}/jobs"),
                no_step_job(10 + id),
            );
            script.rule(
                &format!("gh api repos/o/r/check-runs/{}/annotations", 10 + id),
                message.clone(),
            );
        }
        let report = check(&script, &checkout_with(&[("a.yml", "on: push\n")])).await;
        assert_eq!(codes(&report.blockers), ["billingBlocked"], "{report:?}");

        // One proven of two: a suspicion.
        let script = healthy();
        script.rule(list, runs(&["failure", "failure"]));
        script.rule("gh api repos/o/r/actions/runs/1/jobs", no_step_job(11));
        script.rule("gh api repos/o/r/check-runs/11/annotations", message);
        script.rule("gh api repos/o/r/actions/runs/2/jobs", no_step_job(12));
        script.rule("gh api repos/o/r/check-runs/12/annotations", Ok(done("[]")));
        let report = check(&script, &checkout_with(&[("a.yml", "on: push\n")])).await;
        assert!(report.ok);
        assert_eq!(codes(&report.warnings), ["billingSuspected"], "{report:?}");

        // The shape alone, with no annotation text: a suspicion too.
        let script = healthy();
        script.rule(list, runs(&["failure", "failure", "failure"]));
        for id in 1..=3 {
            script.rule(
                &format!("gh api repos/o/r/actions/runs/{id}/jobs"),
                no_step_job(10 + id),
            );
            script.rule(
                &format!("gh api repos/o/r/check-runs/{}/annotations", 10 + id),
                Ok(done("[]")),
            );
        }
        let report = check(&script, &checkout_with(&[("a.yml", "on: push\n")])).await;
        assert!(report.ok);
        assert_eq!(codes(&report.warnings), ["billingSuspected"], "{report:?}");

        // A run that passed clears it.
        let script = healthy();
        script.rule(list, runs(&["failure", "success", "failure"]));
        let report = check(&script, &checkout_with(&[("a.yml", "on: push\n")])).await;
        assert_eq!(report.verdict, "ready", "{report:?}");
    }

    #[tokio::test]
    async fn a_report_is_kept_per_repository_and_branch_prefix_until_forced() {
        let script = healthy();
        let preflight = GithubPreflight::with_runner(script.clone());
        let dir = checkout_with(&[]);
        let first = preflight
            .check(dir.path(), "o/r", Some("zeron/a"), false)
            .await;
        let calls = script.calls();
        assert!(calls > 0);
        let again = preflight
            .check(dir.path(), "O/R", Some("zeron/b"), false)
            .await;
        assert_eq!(
            script.calls(),
            calls,
            "the same repository and prefix is not asked again"
        );
        assert_eq!(first, again);
        preflight
            .check(dir.path(), "o/r", Some("zeron/a"), true)
            .await;
        assert!(script.calls() > calls, "force asks again");
        let other = preflight
            .check(dir.path(), "o/r", Some("feature/a"), false)
            .await;
        assert_eq!(
            other.branch, "feature/a",
            "another prefix is its own report"
        );
    }

    #[tokio::test]
    async fn a_name_that_is_not_a_repository_is_not_checked() {
        let script = healthy();
        let report = run_checks(&*script, Path::new("."), "o/r/../x", "zeron/x").await;
        assert!(!report.supported);
        assert!(report.ok);
        assert_eq!(script.calls(), 0);
        assert!(
            PreflightReport::unsupported("not on GitHub")
                .verdict
                .starts_with("not checked")
        );
    }
}
