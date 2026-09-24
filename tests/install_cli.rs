use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = PathBuf::from(format!(
            "/tmp/bl-cli-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for dir in [".claude", ".codex", ".ballast"] {
            fs::create_dir_all(path.join(dir)).unwrap();
        }
        fs::write(
            path.join(".ballast/config.toml"),
            "notifications = false\nmode = \"observe\"\nrecovery_sweep_markers = []\n",
        )
        .unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ballast"));
        command
            .env("HOME", &self.0)
            .env("BALLAST_HOME", self.0.join(".ballast"))
            .env("CLAUDE_CONFIG_DIR", self.0.join(".claude"))
            .env("CODEX_HOME", self.0.join(".codex"))
            .env("BALLAST_SERVICE_DIR", self.0.join("services"))
            .env(
                "BALLAST_SERVICE_LABEL",
                format!(
                    "dev.ballast.cli.{}",
                    self.0.file_name().unwrap().to_string_lossy()
                ),
            )
            // No service manager on PATH: a deterministic sandbox-style blocked step, never a real service.
            .env("PATH", "");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn json(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn agent_contract_refuses_previews_validates_and_reports_blocked_apply() {
    let home = Home::new();
    for operation in ["install", "uninstall"] {
        let output = home.run(&[operation]);
        assert_eq!(output.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&output.stdout).contains("--dry-run --json"));
        let refused = json(&home.run(&[operation, "--json"]), 3);
        assert_eq!(refused["status"], "refused");
        let plan = json(&home.run(&[operation, "--dry-run", "--json"]), 0);
        assert_eq!(plan["schema_version"], 1);
        assert_eq!(plan["operation"], operation);
        assert!(!home.0.join("services").exists());
        assert!(!home.0.join(".ballast/run").exists());
        assert_eq!(fs::read_dir(home.0.join(".claude")).unwrap().count(), 0);
        if operation == "install" {
            assert_eq!(
                plan["detected_agents"],
                serde_json::json!(["claude", "codex"])
            );
            assert!(
                plan["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|i| i["files"][0]["diff"].as_str().unwrap().contains("@@"))
            );
            assert_eq!(plan["requires_user_action"].as_array().unwrap().len(), 2);
        }
    }
    assert_eq!(
        json(&home.run(&["uninstall", "--yes", "--json"]), 2)["status"],
        "no_change"
    );
    fs::write(home.0.join(".codex/hooks.json"), "{").unwrap();
    let invalid = json(&home.run(&["install", "--yes", "--json"]), 4);
    assert_eq!(invalid["status"], "invalid");
    assert!(!home.0.join("services").exists());
    assert!(!home.0.join(".claude/settings.json").exists());
    fs::remove_file(home.0.join(".codex/hooks.json")).unwrap();
    let partial = json(&home.run(&["install", "--yes", "--json"]), 5);
    assert_eq!(partial["status"], "partial_failure");
    assert_eq!(partial["items"][0]["status"], "failed");
    assert!(
        partial["items"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("outside the sandbox")
    );
    assert_eq!(partial["items"][1]["status"], "skipped");
    let doctor = json(&home.run(&["doctor", "--json"]), 1);
    assert_eq!(doctor["schema_version"], 1);
    assert_eq!(doctor["operation"], "doctor");
    assert!(
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "daemon" && c["status"] == "failed")
    );
}

#[test]
fn purge_rejects_home_aliases_before_writing() {
    let home = Home::new();
    let sentinel = home.0.join("keep-me");
    fs::write(&sentinel, "unrelated user data").unwrap();
    for ballast_home in [
        home.0.join(".ballast/.."),
        fs::canonicalize(&home.0).unwrap(),
    ] {
        for mode in ["--dry-run", "--yes"] {
            let output = home
                .command()
                .args(["uninstall", "--purge", mode, "--json"])
                .env("BALLAST_HOME", &ballast_home)
                .output()
                .unwrap();
            let result = json(&output, 4);
            assert_eq!(result["status"], "invalid");
            assert_eq!(
                fs::read_to_string(&sentinel).unwrap(),
                "unrelated user data"
            );
            assert!(!home.0.join("run").exists());
        }
    }
}

#[test]
fn removing_hooks_reports_restart_in_plan_and_result() {
    let home = Home::new();
    let hooks = home.0.join(".claude/settings.json");
    fs::write(&hooks, r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true # ballast-managed-hook"}]}]}}"#).unwrap();
    let plan = json(&home.run(&["uninstall", "--dry-run", "--json"]), 0);
    assert_eq!(plan["current_session_needs_restart"], true);
    let result = json(&home.run(&["uninstall", "--yes", "--json"]), 0);
    assert_eq!(result["current_session_needs_restart"], true);
    assert!(
        result["next_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().contains("restart"))
    );
    assert!(
        !fs::read_to_string(hooks)
            .unwrap()
            .contains("ballast-managed-hook")
    );
}
