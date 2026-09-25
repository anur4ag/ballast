use super::*;
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;

#[derive(Clone, Copy, Default, clap::Args)]
pub struct Options {
    /// Apply the displayed plan without prompting (requires prior user approval).
    #[arg(long, conflicts_with = "dry_run")]
    pub yes: bool,
    /// Print the plan and full diff without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Print a versioned machine-readable plan or result.
    #[arg(long)]
    pub json: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Plan {
    pub schema_version: u32,
    pub operation: String,
    pub detected_agents: Vec<String>,
    pub items: Vec<Item>,
    pub requires_user_action: Vec<String>,
    pub current_session_needs_restart: bool,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Item {
    pub id: String,
    pub purpose: String,
    pub selected: bool,
    pub changed: bool,
    pub detail: String,
    pub existing_hooks_kept: usize,
    pub files: Vec<FileChange>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct FileChange {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
    pub diff: String,
    pub backup: Option<PathBuf>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Check {
    pub name: String,
    pub status: String,
    pub detail: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ItemResult {
    id: String,
    status: String,
    reason: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct ResultReport {
    schema_version: u32,
    operation: String,
    status: String,
    items: Vec<ItemResult>,
    doctor: Vec<Check>,
    next_steps: Vec<String>,
    current_session_needs_restart: bool,
}

impl FileChange {
    fn new(
        path: PathBuf,
        original: Option<Vec<u8>>,
        after: Option<String>,
        backup: bool,
    ) -> io::Result<Self> {
        let before = original
            .map(String::from_utf8)
            .transpose()
            .map_err(|_| io::Error::other(format!("{} is not UTF-8", path.display())))?;
        let mut result = Self {
            path,
            before,
            after,
            ..Self::default()
        };
        if result.before != result.after {
            let old = result.before.as_deref().unwrap_or_default();
            let new = result.after.as_deref().unwrap_or_default();
            // A full-file unified hunk is exact, handles arbitrary JSON, and needs no diff dependency.
            result.diff = format!(
                "--- {}\n+++ {}\n@@ -{},{} +{},{} @@\n",
                if result.before.is_some() {
                    result.path.to_string_lossy().into_owned()
                } else {
                    "/dev/null".into()
                },
                if result.after.is_some() {
                    result.path.to_string_lossy().into_owned()
                } else {
                    "/dev/null".into()
                },
                usize::from(!old.is_empty()),
                old.lines().count(),
                usize::from(!new.is_empty()),
                new.lines().count()
            );
            for (prefix, text) in [('-', old), ('+', new)] {
                for line in text.split_terminator('\n') {
                    result.diff.push(prefix);
                    result.diff.push_str(line);
                    result.diff.push('\n');
                }
                if !text.is_empty() && !text.ends_with('\n') {
                    result.diff.push_str("\\ No newline at end of file\n");
                }
            }
            if backup && result.before.is_some() {
                let seconds = unsafe { libc::time(std::ptr::null_mut()) };
                let mut local: libc::tm = unsafe { std::mem::zeroed() };
                if unsafe { libc::localtime_r(&seconds, &mut local) }.is_null() {
                    return Err(io::Error::other("cannot determine local backup time"));
                }
                let stamp = format!(
                    "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}",
                    local.tm_year + 1900,
                    local.tm_mon + 1,
                    local.tm_mday,
                    local.tm_hour,
                    local.tm_min,
                    local.tm_sec
                );
                let mut collision = 0;
                loop {
                    let suffix = if collision == 0 {
                        String::new()
                    } else {
                        format!("-{collision}")
                    };
                    let path = result.path.with_file_name(format!(
                        "{}.ballast-{stamp}{suffix}.bak",
                        result.path.file_name().unwrap().to_string_lossy()
                    ));
                    match fs::symlink_metadata(&path) {
                        Ok(_) => collision += 1,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            result.backup = Some(path);
                            break;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(result)
    }

    pub(super) fn from_edit(edit: ConfigEdit) -> io::Result<Self> {
        let after = if edit.changed {
            Some(format!("{}\n", serde_json::to_string_pretty(&edit.value)?))
        } else {
            edit.original
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
        };
        Self::new(edit.path, edit.original, after, true)
    }

    fn preflight(&self) -> io::Result<()> {
        if read_regular(&self.path)?.as_deref() != self.before.as_deref().map(str::as_bytes) {
            return Err(io::Error::other(format!(
                "{} changed after planning; retry",
                self.path.display()
            )));
        }
        if self.before != self.after {
            writable_parent(&self.path)?;
            if self.backup.as_ref().is_some_and(|p| p.exists()) {
                return Err(io::Error::other("planned backup already exists; retry"));
            }
        }
        Ok(())
    }

    pub(super) fn save(&self) -> io::Result<()> {
        self.preflight()?;
        if self.before == self.after {
            return Ok(());
        }
        if let (Some(backup), Some(original)) = (&self.backup, &self.before) {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(backup)?;
            file.write_all(original.as_bytes())?;
            file.sync_all()?;
        }
        match &self.after {
            Some(text) => atomic_write(&self.path, text.as_bytes()),
            None => remove_if_exists(&self.path),
        }
    }
}

fn writable_parent(path: &Path) -> io::Result<()> {
    let mut parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent directory"))?;
    while !parent.exists() {
        parent = parent
            .parent()
            .ok_or_else(|| io::Error::other("missing parent directory"))?;
    }
    let cpath = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes())?;
    if !parent.is_dir() || unsafe { libc::access(cpath.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
        return Err(io::Error::other(format!(
            "{}: cannot write {}; run outside the sandbox or approve directory access",
            path.display(),
            parent.display()
        )));
    }
    Ok(())
}

impl Installation {
    pub(super) fn detected(&self, path: &Path, agent: &str) -> bool {
        path.parent().is_some_and(Path::is_dir)
            || resolve_invocation(
                Path::new(agent),
                &std::env::var_os("PATH").unwrap_or_default(),
            )
            .ok()
            .flatten()
            .is_some()
    }

    fn check_purge_target(&self) -> io::Result<()> {
        let home = fs::canonicalize(&self.home)?;
        match fs::canonicalize(&self.paths.base) {
            Ok(base) if home.starts_with(&base) => Err(io::Error::other(
                "refusing to purge HOME or its ancestor; choose a dedicated BALLAST_HOME",
            )),
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    pub(super) fn plan(&self, installing: bool, purge: bool) -> io::Result<Plan> {
        self.validate()?;
        if purge {
            self.check_purge_target()?;
        }
        let edits = self.edits(installing)?;
        self.check_service_owner(installing)?;
        crate::daemon::files::Config::load(&self.paths)?;
        let service = FileChange::new(
            self.service_file(),
            read_regular(&self.service_file())?,
            installing.then(|| self.service_text()),
            false,
        )?;
        let changed = service.before != service.after
            || if installing {
                !self.service_running() || !self.reachable()
            } else {
                self.service_running() || recovery::needs_recovery(&self.paths)
            };
        let mut plan = Plan {
            schema_version: 1,
            operation: if installing { "install" } else { "uninstall" }.into(),
            items: vec![Item {
                id: "service".into(),
                selected: true,
                changed,
                purpose: if installing {
                    "Start a background service (runs as you, never root)"
                } else {
                    "Resumes anything Ballast paused, then stops the service"
                }
                .into(),
                detail: if installing {
                    format!(
                        "Private runtime, state and log directories: {}",
                        self.paths.base.display()
                    )
                } else {
                    if purge {
                        format!(
                            "Deletes {} after resuming paused work",
                            self.paths.base.display()
                        )
                    } else {
                        format!(
                            "Keeps {} (add --purge to delete it)",
                            self.paths.base.display()
                        )
                    }
                },
                files: vec![service],
                ..Item::default()
            }],
            ..Plan::default()
        };
        for edit in edits {
            let agent = if edit.path == self.claude_dir.join("settings.json") {
                "claude"
            } else {
                "codex"
            };
            if self.detected(&edit.path, agent) {
                plan.detected_agents.push(agent.into());
            }
            let existing_hooks_kept = edit.value["hooks"]
                .as_object()
                .into_iter()
                .flat_map(|o| o.values())
                .filter_map(Value::as_array)
                .flatten()
                .flat_map(|g| g["hooks"].as_array().unwrap())
                .filter(|h| !owned(h))
                .count();
            let file = FileChange::from_edit(edit)?;
            if !installing && file.before == file.after {
                continue;
            }
            let changed = file.before != file.after;
            let repair = installing && file.before.as_ref().is_some_and(|b| b.contains(MARKER));
            plan.items.push(Item {
                id: agent.into(),
                selected: true,
                changed,
                existing_hooks_kept,
                purpose: format!(
                    "{} {} hooks",
                    if installing {
                        "Set up"
                    } else {
                        "Remove Ballast"
                    },
                    if agent == "claude" {
                        "Claude Code"
                    } else {
                        "Codex"
                    }
                ),
                detail: if !changed {
                    String::new()
                } else if repair {
                    "Already set up; repairing hook paths or events".into()
                } else {
                    String::new()
                },
                files: vec![file],
            });
        }
        if purge {
            plan.items.push(Item {
                id: "purge".into(),
                selected: true,
                changed: true,
                purpose: format!(
                    "Delete retained Ballast configuration, state and logs: {}",
                    self.paths.base.display()
                ),
                ..Item::default()
            });
        }
        plan.refresh_actions();
        Ok(plan)
    }

    pub(super) fn apply(&self, plan: &Plan) -> (ResultReport, i32) {
        let mut report = ResultReport {
            schema_version: 1,
            operation: plan.operation.clone(),
            status: "success".into(),
            ..ResultReport::default()
        };
        let installing = plan.operation == "install";
        let preflight = (|| {
            self.check_service_owner(installing)?;
            if plan.items.iter().any(|i| i.id == "purge" && i.selected) {
                self.check_purge_target()?;
            }
            for item in plan.items.iter().filter(|i| i.selected) {
                for file in &item.files {
                    file.preflight()?;
                }
            }
            if plan
                .items
                .iter()
                .any(|i| i.id == "service" && i.selected && (i.changed || !installing))
            {
                writable_parent(&self.paths.base.join("run"))?;
                for dir in [
                    &self.paths.base,
                    &self.paths.base.join("run"),
                    &self.paths.base.join("state"),
                    &self.paths.base.join("log"),
                ] {
                    if fs::symlink_metadata(dir).is_ok_and(|m| !m.is_dir()) {
                        return Err(io::Error::other(format!(
                            "{} must be a directory, not a symlink",
                            dir.display()
                        )));
                    }
                }
            }
            Ok::<_, io::Error>(())
        })();
        if let Err(error) = preflight {
            return (
                error_report(&plan.operation, "invalid", error.to_string()),
                4,
            );
        }
        let mut failed = false;
        let mut applied = false;
        for item in &plan.items {
            let mut result = ItemResult {
                id: item.id.clone(),
                status: "skipped".into(),
                reason: "nothing to change".into(),
            };
            if !item.selected {
                result.reason = "not selected".into();
            } else if failed {
                result.reason = "previous step failed; rerun after fixing it".into();
            } else if item.changed || (!installing && item.id == "service") {
                let action = (|| {
                    let mut changed = item.changed;
                    if item.id == "service" {
                        if installing {
                            self.paths.prepare()?;
                        } else {
                            changed |= daemon::resume_command(&self.paths, None)? > 0;
                            self.stop_service()?;
                            changed |= daemon::resume_command(&self.paths, None)? > 0;
                        }
                    }
                    let _lock = if !installing && self.paths.base.exists() {
                        Some(ipc::lock(&self.paths)?)
                    } else {
                        None
                    };
                    if !installing && recovery::needs_recovery(&self.paths) {
                        return Err(io::Error::other(
                            "frozen work remains; run `ballast resume --all` before uninstalling",
                        ));
                    }
                    for file in &item.files {
                        file.save()?;
                    }
                    if item.id == "service" {
                        if installing {
                            self.start_service()?;
                            let deadline = Instant::now() + Duration::from_secs(10);
                            while !self.reachable() {
                                if Instant::now() >= deadline {
                                    return Err(io::Error::other(
                                        "service installed but daemon unreachable; run `ballast doctor` and inspect the user service log",
                                    ));
                                }
                                std::thread::sleep(Duration::from_millis(100));
                            }
                        } else {
                            #[cfg(target_os = "linux")]
                            if item.changed {
                                self.service_command(&["daemon-reload"])?;
                            }
                        }
                    }
                    if item.id == "purge" {
                        self.check_purge_target()?;
                        fs::remove_dir_all(&self.paths.base)?;
                    }
                    Ok::<_, io::Error>(changed)
                })();
                match action {
                    Ok(changed) => {
                        if changed {
                            result.status = "applied".into();
                            result.reason = item.purpose.clone();
                            applied = true;
                        } else {
                            result.reason =
                                "Recovery checked; no stopped work or configuration changes."
                                    .into();
                        }
                    }
                    Err(error) => {
                        result.status = "failed".into();
                        result.reason = format!(
                            "{}: {error}; if sandbox access is blocked, run outside the sandbox or approve this step",
                            item.id
                        );
                        failed = true;
                    }
                }
            }
            report.items.push(result);
        }
        report.current_session_needs_restart = report
            .items
            .iter()
            .any(|i| matches!(i.id.as_str(), "claude" | "codex") && i.status == "applied");
        if report.current_session_needs_restart {
            report.next_steps.push(
                "Hooks take effect in new agent sessions; restart your current agent session."
                    .into(),
            );
        }
        if installing {
            report.doctor = self.doctor_checks(false);
            for check in &mut report.doctor {
                let component = match check.name.as_str() {
                    "user service" | "daemon" => Some("service"),
                    "claude hooks" => Some("claude"),
                    "codex hooks" | "Codex trust" => Some("codex"),
                    _ => None,
                };
                if component.is_some_and(|id| plan.items.iter().any(|i| i.id == id && !i.selected))
                {
                    check.status = "skipped".into();
                    check.detail = if component == Some("service") {
                        "Service setup was not selected; installed hooks stay inactive until a daemon runs."
                    } else {
                        "Hook setup was not selected; existing configuration was left untouched."
                    }.into();
                }
            }
            // Trust is deliberately left to the user; any other failed check means setup is incomplete.
            failed |= report
                .doctor
                .iter()
                .any(|c| c.status == "failed" && c.name != "notifications");
            if plan.items.iter().any(|i| i.id == "codex" && i.selected)
                && self.codex_trusted().is_err()
            {
                report.next_steps.push("Open Codex and approve the Ballast hooks yourself with /hooks; Ballast never writes trust.".into());
            } else {
                report
                    .next_steps
                    .push("Run `ballast top` to watch it work.".into());
            }
        } else {
            let removed =
                !self.service_running() && !self.reachable() && !self.service_file().exists();
            report.doctor.push(Check {
                name: "user service".into(),
                status: if removed { "ok" } else { "failed" }.into(),
                detail: if removed {
                    "stopped and removed"
                } else {
                    "still installed or running; rerun uninstall"
                }
                .into(),
            });
            match self.edits(false) {
                Ok(edits) => {
                    for edit in edits {
                        report.doctor.push(Check {
                            name: edit.path.display().to_string(),
                            status: if edit.changed { "failed" } else { "ok" }.into(),
                            detail: if edit.changed {
                                "Ballast hooks remain; rerun uninstall"
                            } else {
                                "no Ballast hooks; other settings retained"
                            }
                            .into(),
                        });
                    }
                }
                Err(error) => report.doctor.push(Check {
                    name: "agent hooks".into(),
                    status: "failed".into(),
                    detail: error.to_string(),
                }),
            }
            failed |= report.doctor.iter().any(|c| c.status == "failed");
            report
                .next_steps
                .push("Remove the package with your package manager when ready.".into());
        }

        if failed {
            report.next_steps = vec!["Resolve the failed steps above, then rerun the preview and approved command; verify with `ballast doctor --json`.".into()];
        }
        let code = if failed {
            report.status = "partial_failure".into();
            5
        } else if !applied {
            report.status = "no_change".into();
            2
        } else {
            0
        };
        (report, code)
    }
}

impl Plan {
    pub(super) fn refresh_actions(&mut self) {
        self.requires_user_action.clear();
        self.current_session_needs_restart = self
            .items
            .iter()
            .any(|i| i.selected && i.changed && matches!(i.id.as_str(), "claude" | "codex"));
        if self.operation == "install" && self.items.iter().any(|i| i.selected && i.id == "codex") {
            self.requires_user_action.push(
                "Approve Ballast hooks yourself in Codex with /hooks; Ballast never writes trust."
                    .into(),
            );
        }
        if self.current_session_needs_restart {
            self.requires_user_action.push(
                "Hooks change in new agent sessions; restart the current agent session.".into(),
            );
        }
    }
}

fn error_report(operation: &str, status: &str, reason: String) -> ResultReport {
    ResultReport {
        schema_version: 1,
        operation: operation.into(),
        status: status.into(),
        items: vec![ItemResult {
            id: "preflight".into(),
            status: "failed".into(),
            reason,
        }],
        ..ResultReport::default()
    }
}

pub fn command(installing: bool, purge: bool, options: Options) -> i32 {
    let operation = if installing { "install" } else { "uninstall" };
    let terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
    let result = if !options.yes && !options.dry_run && (!terminal || options.json) {
        Err((
            3,
            format!(
                "Run `ballast {operation} --dry-run --json` and show the plan to the user; after the user approves, run `ballast {operation} --yes`. A terminal is required without --yes or --dry-run."
            ),
        ))
    } else {
        (|| {
            let install = Installation::from_env().map_err(|e| (4, e.to_string()))?;
            let mut plan = install
                .plan(installing, purge)
                .map_err(|e| (4, e.to_string()))?;
            if options.dry_run {
                if options.json {
                    print_json(&plan);
                } else {
                    ui::print_plan(&plan, &install.home);
                    println!("{}", ui::diff(&plan));
                }
                return Ok(0);
            }
            if !options.yes
                && !ui::confirm(&mut plan, &install.home).map_err(|e| (3, e.to_string()))?
            {
                return Err((3, "Cancelled; nothing written.".into()));
            }
            let (report, code) = install.apply(&plan);
            render_result(&report, options.json);
            Ok(code)
        })()
    };
    match result {
        Ok(code) => code,
        Err((code, reason)) => {
            render_result(
                &error_report(
                    operation,
                    if code == 4 { "invalid" } else { "refused" },
                    reason,
                ),
                options.json,
            );
            code
        }
    }
}

pub fn doctor_command(notify: bool, json: bool) -> i32 {
    match Installation::from_env() {
        Ok(install) => {
            let checks = install.doctor_checks(notify);
            let healthy = checks.iter().all(|c| c.status == "ok");
            if json {
                print_json(
                    &serde_json::json!({"schema_version":1,"operation":"doctor","status":if healthy {"healthy"} else {"attention_required"},"checks":checks}),
                );
            } else {
                for check in checks {
                    println!(
                        "{} {}: {}",
                        check.status.to_uppercase(),
                        check.name,
                        check.detail
                    );
                }
            }
            i32::from(!healthy)
        }
        Err(e) => {
            render_result(&error_report("doctor", "invalid", e.to_string()), json);
            4
        }
    }
}

fn print_json(value: &impl Serialize) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}
fn render_result(report: &ResultReport, json: bool) {
    if json {
        print_json(report);
        return;
    }
    let mut text = format!("Ballast {}: {}\n", report.operation, report.status);
    for item in &report.items {
        text += &format!(
            "{} {}: {}\n",
            item.status.to_uppercase(),
            item.id,
            item.reason
        );
    }
    for check in &report.doctor {
        text += &format!(
            "{} {}: {}\n",
            check.status.to_uppercase(),
            check.name,
            check.detail
        );
    }
    if report.current_session_needs_restart {
        text += "Hooks take effect in new sessions; restart your current agent session.\n";
    }
    if let Some(step) = report.next_steps.last() {
        text += &format!("Next: {step}\n");
    }
    ui::print_text(&text);
}
