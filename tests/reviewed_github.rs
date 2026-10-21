//! Explicit local saves against an isolated fake GitHub; no live forge calls.
#![cfg(unix)]

mod common;

use common::Repo;
use herdr_reviewr::app::App;
use herdr_reviewr::model::Scope;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

struct GitHub {
    dir: tempfile::TempDir,
}

impl GitHub {
    fn new() -> Self {
        // Host tools outrank PATH; never let a fake-provider test reach an installed gh.
        assert!(
            !["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh", "/bin/gh"]
                .iter()
                .any(|path| std::path::Path::new(path).exists()),
            "run fake GitHub tests in an environment without host gh"
        );
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        fs::write(&gh, include_str!("fixtures/fake_gh_viewed.py")).unwrap();
        fs::set_permissions(gh, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn respond(&self, fixture: &Value) {
        fs::write(self.dir.path().join("responses.json"), fixture.to_string()).unwrap();
        fs::write(self.dir.path().join("calls.jsonl"), "").unwrap();
    }

    fn mark(&self, repo: &Repo, paths: &[&str]) -> Output {
        let path = std::env::join_paths(
            std::iter::once(self.dir.path().to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        Command::new(env!("CARGO_BIN_EXE_herdr-reviewr"))
            .current_dir(repo.path())
            .arg("--mark-reviewed")
            .args(paths)
            .env("PATH", path)
            .env("FAKE_GH_VIEWED_DIR", self.dir.path())
            .env("HERDR_PLUGIN_CONFIG_DIR", self.dir.path())
            .env("HERDR_BIN_PATH", "false")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_SOCKET_PATH")
            .output()
            .unwrap()
    }

    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn worktree() -> Repo {
    let repo = Repo::init();
    repo.write("a file.txt", "one\n");
    repo.commit_all("base");
    repo.git(&["remote", "add", "origin", "https://github.com/me/widgets.git"]);
    repo.set_origin_default("main", "main");
    repo.git(&["switch", "-qc", "work"]);
    repo.git(&["config", "branch.work.remote", "origin"]);
    repo.git(&["config", "branch.work.merge", "refs/heads/published"]);
    repo.write("a file.txt", "two\n");
    repo
}

fn candidate(number: u64, state: &str, cross: bool, owner: &str) -> Value {
    json!({"number": number, "state": state, "headRefName": "published",
        "createdAt": format!("2026-10-{number:02}T00:00:00Z"),
        "isCrossRepository": cross, "headRepository": {"nameWithOwner": owner}})
}

fn fixture() -> Value {
    json!({"lookup": {"me/widgets": {"o": [candidate(2, "OPEN", false, "me/widgets")]}},
        "detail": {"me/widgets#2": {"id": "PR_fork", "state": "OPEN"}}})
}

#[test]
#[ignore = "requires an isolated environment without host gh"]
fn cli_mirrors_idempotently_to_the_open_fork_pr_not_upstream_history_or_a_namesake() {
    let repo = worktree();
    repo.write("second.txt", "second\n");
    repo.git(&["remote", "add", "upstream", "https://github.com/acme/widgets.git"]);
    let gh = GitHub::new();
    let mut fixture = fixture();
    fixture["lookup"]["acme/widgets"] = json!({
        "o": [candidate(9, "OPEN", true, "stranger/widgets")],
        "h": [candidate(1, "CLOSED", true, "me/widgets")]
    });
    fixture["lookup"]["acme/widgets"]["h"][0]["headRefOid"] =
        json!(repo.git(&["rev-parse", "HEAD"]).trim());
    gh.respond(&fixture);
    for _ in 0..2 {
        let output = gh.mark(&repo, &["a file.txt", "second.txt"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let calls = gh.calls();
    let writes: Vec<_> = calls
        .iter()
        .filter(|call| call["query"].as_str().unwrap().starts_with("mutation"))
        .collect();
    assert_eq!(writes.len(), 4);
    for (write, path) in writes.iter().zip(["a file.txt", "second.txt"].repeat(2)) {
        assert_eq!(write["id"], "PR_fork");
        assert_eq!(write["path"], path);
    }
    let root = herdr_reviewr::git::toplevel(repo.path()).unwrap();
    assert!(App::new(root, Scope::Uncommitted, None).is_reviewed("a file.txt"));
}

#[test]
#[ignore = "requires an isolated environment without host gh"]
fn github_errors_warn_but_local_marks_survive_and_no_open_pr_stays_local() {
    let repo = worktree();
    let gh = GitHub::new();
    for response in
        [json!({"errors": [{"message": "offline"}]}), json!({"data": {"markFileAsViewed": null}})]
    {
        let mut fixture = fixture();
        fixture["mutation"] = response;
        gh.respond(&fixture);
        let output = gh.mark(&repo, &["a file.txt"]);
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("warning: marks saved locally"));
        let root = herdr_reviewr::git::toplevel(repo.path()).unwrap();
        assert!(App::new(root, Scope::Uncommitted, None).is_reviewed("a file.txt"));
    }
    gh.respond(
        &json!({"lookup": {"me/widgets": {"h": [candidate(2, "CLOSED", false, "me/widgets")]}}}),
    );
    let output = gh.mark(&repo, &["a file.txt"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(gh.calls().iter().all(|call| !call["query"].as_str().unwrap().starts_with("mutation")));
    repo.git(&["remote", "set-url", "origin", "https://gitlab.com/me/widgets.git"]);
    gh.respond(&json!({}));
    assert!(gh.mark(&repo, &["a file.txt"]).status.success());
    assert!(gh.calls().is_empty());
}

#[test]
#[ignore = "requires an isolated environment without host gh"]
fn pins_use_their_own_repository_closed_pins_skip_and_stale_pins_fall_back() {
    let repo = worktree();
    repo.git(&["remote", "add", "pinned", "https://github.com/other/widgets.git"]);
    repo.git(&["config", "branch.work.remote", "pinned"]);
    repo.git(&["config", "branch.work.merge", "refs/pull/7/head"]);
    repo.git(&["config", "branch.work.pushRemote", "origin"]);
    let gh = GitHub::new();
    for state in ["OPEN", "CLOSED", "MERGED"] {
        gh.respond(&json!({"detail": {"other/widgets#7": {"id": "PR_pin", "state": state}}}));
        let output = gh.mark(&repo, &["a file.txt"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
        let calls = gh.calls();
        assert_eq!(calls.len(), if state == "OPEN" { 2 } else { 1 });
        if state == "OPEN" {
            assert_eq!(calls[1]["id"], "PR_pin");
        }
    }
    for missing in [Value::Null, json!({"errors": [{"type": "NOT_FOUND", "message": "gone"}]})] {
        let mut fixture = fixture();
        fixture["lookup"]["me/widgets"]["o"][0]["headRefName"] = json!("work");
        fixture["detail"]["other/widgets#7"] = missing;
        gh.respond(&fixture);
        let output = gh.mark(&repo, &["a file.txt"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(gh.calls().last().unwrap()["id"], "PR_fork");
    }
}
