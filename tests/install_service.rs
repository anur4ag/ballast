#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct TestService {
    home: PathBuf,
    label: String,
}
impl TestService {
    fn command(&self, action: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ballast"));
        command
            .arg(action)
            .env("HOME", &self.home)
            .env("BALLAST_HOME", self.home.join(".ballast"))
            .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("BALLAST_SERVICE_DIR", self.home.join("services"))
            .env("BALLAST_SERVICE_LABEL", &self.label);
        command
    }
    fn target(&self) -> String {
        format!("gui/{}/{}", unsafe { libc::geteuid() }, self.label)
    }
}
impl Drop for TestService {
    fn drop(&mut self) {
        let _ = Command::new("launchctl")
            .args(["bootout", &self.target()])
            .output();
        let _ = fs::remove_dir_all(&self.home);
    }
}

#[test]
#[ignore = "loads a unique temporary LaunchAgent; run explicitly in a macOS GUI session"]
fn uninstall_recovers_when_the_loaded_services_plist_is_missing() {
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let service = TestService {
        home: PathBuf::from(format!("/tmp/blt-install-{unique}")),
        label: format!("dev.ballast.test.{unique}"),
    };
    fs::create_dir_all(service.home.join(".ballast")).unwrap();
    fs::write(
        service.home.join(".ballast/config.toml"),
        "mode = \"observe\"\nnotifications = false\nrecovery_sweep_markers = []\n",
    )
    .unwrap();
    let output = service.command("install").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_file(
        service
            .home
            .join("services")
            .join(format!("{}.plist", service.label)),
    )
    .unwrap();
    let output = service.command("install").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already loaded"));
    let output = service.command("uninstall").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !Command::new("launchctl")
            .args(["print", &service.target()])
            .output()
            .unwrap()
            .status
            .success()
    );
    for file in [".claude/settings.json", ".codex/hooks.json"] {
        assert!(
            !fs::read_to_string(service.home.join(file))
                .unwrap()
                .contains("ballast-managed-hook")
        );
    }
    let output = service.command("uninstall").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
