use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(Installation);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let home = PathBuf::from(format!(
            "/tmp/bl-install-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&home).unwrap();
        Self(Installation {
            paths: Paths {
                base: home.join(".ballast"),
            },
            binary: home.join("bin/ballast"),
            claude_dir: home.join(".claude"),
            codex_dir: home.join(".codex"),
            service_dir: home.join("services"),
            service_label: "dev.ballast.test".into(),
            home,
        })
    }
    fn merge(&self, install: bool) {
        for edit in self.0.edits(install).unwrap() {
            edit.save().unwrap();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0.home);
    }
}

#[test]
fn config_round_trip_preserves_other_hooks_and_backs_up_only_changes() {
    let mut fixture = Fixture::new();
    let unrelated = json!({"model": "keep", "hooks": {"UnrelatedEmpty": [],"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo ballast hook claude"}]}], "Stop": [{"hooks": [{"type": "prompt", "prompt": "keep"}]}]}});
    for (path, _) in fixture.0.config_files() {
        atomic_write(&path, &serde_json::to_vec(&unrelated).unwrap()).unwrap();
    }
    fixture.merge(true);
    let first = fs::read(fixture.0.codex_dir.join("hooks.json")).unwrap();
    fixture.merge(true);
    assert_eq!(
        fs::read(fixture.0.codex_dir.join("hooks.json")).unwrap(),
        first
    );
    assert_eq!(fs::read_dir(&fixture.0.codex_dir).unwrap().count(), 2);
    for (path, agent) in fixture.0.config_files() {
        fixture.0.hooks_current(&path, agent).unwrap();
    }
    let value: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(value["hooks"]["PreToolUse"][1]["hooks"][0]["timeout"], 600);
    assert_eq!(value["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 1);
    fixture.0.binary = fixture.0.home.join("moved ' binary/$ballast");
    assert!(
        fixture
            .0
            .hooks_current(&fixture.0.codex_dir.join("hooks.json"), "codex")
            .is_err()
    );
    fixture.merge(true);
    for (path, agent) in fixture.0.config_files() {
        fixture.0.hooks_current(&path, agent).unwrap();
    }
    fixture.merge(false);
    for (path, _) in fixture.0.config_files() {
        assert_eq!(ConfigEdit::read(path).unwrap().value, unrelated);
    }
    fixture.merge(false);
}

#[test]
fn fresh_install_and_mixed_group_uninstall() {
    let fixture = Fixture::new();
    fixture.merge(true);
    let path = fixture.0.claude_dir.join("settings.json");
    let mut edit = ConfigEdit::read(path.clone()).unwrap();
    edit.value["hooks"]["PreToolUse"][0]["hooks"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "command", "command": "keep-me"}));
    edit.changed = true;
    edit.save().unwrap();
    fixture.merge(false);
    let value = ConfigEdit::read(path).unwrap().value;
    assert_eq!(
        value["hooks"],
        json!({"PreToolUse": [{"matcher": "Bash|Monitor", "hooks": [{"type": "command", "command": "keep-me"}]}]})
    );
}

#[test]
fn malformed_second_config_never_writes_the_first_or_the_service() {
    let fixture = Fixture::new();
    let claude = fixture.0.claude_dir.join("settings.json");
    let codex = fixture.0.codex_dir.join("hooks.json");
    atomic_write(&claude, b"{\"keep\": true}").unwrap();
    for malformed in [
        "{",
        "[]",
        "{\"hooks\":[]}",
        "{\"hooks\":{\"Stop\":[{\"hooks\":null}]}}",
        "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":1}]}]}}",
        "{\"hooks\":{\"Stop\":[{\"matcher\":false,\"hooks\":[]}]}}",
    ] {
        atomic_write(&codex, malformed.as_bytes()).unwrap();
        let error = fixture.0.install().unwrap_err().to_string();
        assert!(error.contains("malformed"));
        assert_eq!(fs::read(&claude).unwrap(), b"{\"keep\": true}");
        assert_eq!(fs::read(&codex).unwrap(), malformed.as_bytes());
        assert!(!fixture.0.service_file().exists());
        assert_eq!(fs::read_dir(&fixture.0.claude_dir).unwrap().count(), 1);
    }
}

#[test]
fn refuse_symlinks_and_detect_concurrent_edit() {
    let fixture = Fixture::new();
    fixture.merge(true);
    let edits = fixture.0.edits(false).unwrap();
    let claude = fixture.0.claude_dir.join("settings.json");
    atomic_write(&claude, b"{\"new\":true}").unwrap();
    assert!(edits.into_iter().next().unwrap().save().is_err());
    fs::remove_file(&claude).unwrap();
    std::os::unix::fs::symlink(fixture.0.codex_dir.join("hooks.json"), &claude).unwrap();
    assert!(fixture.0.edits(true).is_err());
}

#[test]
fn codex_trust_is_read_only_and_invalidated_by_a_move_or_disable() {
    let mut fixture = Fixture::new();
    fixture.merge(true);
    assert!(fixture.0.codex_trusted().is_err());
    let mut text = String::new();
    for (event, key_event) in EVENTS {
        let group = fixture.0.hook_group(event, "codex");
        let key = format!(
            "{}:{key_event}:0:0",
            fs::canonicalize(fixture.0.codex_dir.join("hooks.json"))
                .unwrap()
                .display()
        );
        text += &format!(
            "[hooks.state.{}]\ntrusted_hash = {:?}\n",
            serde_json::to_string(&key).unwrap(),
            codex_hash(key_event, &group, &group["hooks"][0])
        );
    }
    let config = fixture.0.codex_dir.join("config.toml");
    atomic_write(&config, text.as_bytes()).unwrap();
    fixture.0.codex_trusted().unwrap();
    assert_eq!(fs::read_to_string(&config).unwrap(), text);
    atomic_write(
        &config,
        format!("[features]\nhooks = false\n{text}").as_bytes(),
    )
    .unwrap();
    assert!(fixture.0.codex_trusted().is_err());
    atomic_write(
        &config,
        text.replace("trusted_hash =", "enabled = false\ntrusted_hash =")
            .as_bytes(),
    )
    .unwrap();
    assert!(fixture.0.codex_trusted().is_err());
    atomic_write(&config, text.as_bytes()).unwrap();
    fixture.0.binary = fixture.0.home.join("new/ballast");
    fixture.merge(true);
    assert!(fixture.0.codex_trusted().is_err());
}

#[test]
fn hook_command_survives_shell_metacharacters_and_sets_the_selected_home() {
    let mut fixture = Fixture::new();
    fixture.0.binary = fixture.0.home.join("a ' $ ` binary");
    atomic_write(
        &fixture.0.binary,
        b"#!/bin/sh\ntest \"$1\" = hook && test \"$2\" = codex && test -d \"$BALLAST_HOME\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fixture.0.binary, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(&fixture.0.paths.base).unwrap();
    let group = fixture.0.hook_group("PreToolUse", "codex");
    let run = || {
        Command::new("sh")
            .arg("-c")
            .arg(group["hooks"][0]["command"].as_str().unwrap())
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    fs::write(
        &fixture.0.binary,
        b"#!/bin/sh\nprintf pass-through\nexit 23\n",
    )
    .unwrap();
    let output = run();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, b"pass-through");
    fs::remove_file(&fixture.0.binary).unwrap();
    let output = run();
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn service_escapes_paths_and_rejects_unsafe_labels() {
    let mut fixture = Fixture::new();
    fixture.0.binary = fixture.0.home.join("bin & \" % $ ballast");
    fixture.0.validate().unwrap();
    let service = fixture.0.service_text();
    #[cfg(target_os = "macos")]
    assert!(service.contains("&amp; &quot; % $"));
    #[cfg(target_os = "linux")]
    assert!(service.contains("ExecStart=/usr/bin/env -- ") && service.contains("& \\\" %% $$"));
    fixture.0.service_label = "../../other".into();
    assert!(fixture.0.validate().is_err());
    fixture.0.service_label = "--other".into();
    assert!(fixture.0.validate().is_err());
}

#[test]
fn upgrade_keeps_later_unrelated_hook_trust_indices() {
    let mut fixture = Fixture::new();
    fixture.merge(true);
    let path = fixture.0.codex_dir.join("hooks.json");
    let extra = json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "true"}]});
    let mut edit = ConfigEdit::read(path.clone()).unwrap();
    edit.value["hooks"]["PreToolUse"]
        .as_array_mut()
        .unwrap()
        .push(extra.clone());
    edit.changed = true;
    edit.save().unwrap();
    fixture.0.binary = fixture.0.home.join("new/ballast");
    fixture.merge(true);
    assert_eq!(
        ConfigEdit::read(path).unwrap().value["hooks"]["PreToolUse"][1],
        extra
    );
}

#[test]
fn long_socket_path_is_rejected_before_any_install_writes() {
    let mut fixture = Fixture::new();
    let limit = if cfg!(target_os = "macos") { 104 } else { 108 };
    let prefix = fixture.0.home.join("x");
    let suffix_len = "/run/ballastd.sock".len();
    fixture.0.paths.base = PathBuf::from(format!(
        "{}{}",
        prefix.display(),
        "x".repeat(limit - 1 - suffix_len - prefix.as_os_str().len())
    ));
    fixture.0.validate().unwrap();
    fixture.0.paths.base.as_mut_os_string().push("x");
    let error = fixture.0.install().unwrap_err().to_string();
    assert!(error.contains(&format!("shorter than {limit} bytes")));
    assert!(error.contains("BALLAST_HOME"));
    assert_eq!(fs::read_dir(&fixture.0.home).unwrap().count(), 0);
}

#[test]
#[cfg(target_os = "linux")]
fn systemd_restarts_are_unlimited_with_bounded_backoff() {
    let text = Fixture::new().0.service_text();
    let unit = text
        .split("[Unit]\n")
        .nth(1)
        .unwrap()
        .split("[Service]")
        .next()
        .unwrap();
    assert!(unit.contains("StartLimitIntervalSec=0\n"));
    let service = text
        .split("[Service]\n")
        .nth(1)
        .unwrap()
        .split("[Install]")
        .next()
        .unwrap();
    for directive in [
        "Restart=always\n",
        "RestartSec=1\n",
        "RestartSteps=10\n",
        "RestartMaxDelaySec=30\n",
    ] {
        assert!(service.contains(directive));
    }
}

#[test]
fn invoked_path_keeps_the_symlink_across_a_cellar_upgrade() {
    let fixture = Fixture::new();
    let bin = fixture.0.home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let link = bin.join("ballast");
    for version in ["1", "2"] {
        let target = fixture
            .0
            .home
            .join(format!("Cellar/ballast/{version}/ballast"));
        atomic_write(&target, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        for invoked in [Path::new("ballast"), link.as_path()] {
            assert_eq!(
                resolve_invocation(invoked, bin.as_os_str()).unwrap(),
                Some(link.clone())
            );
        }
    }
}
