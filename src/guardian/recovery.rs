use super::FrozenWorkload;
use crate::attribution::Marker;
use crate::daemon::files::{Paths, RotatingLog};
use crate::platform::{Platform, ProcessIdentity, Signal};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;

#[derive(Serialize, Deserialize)]
pub struct FrozenState {
    pub boot_id: String,
    pub workloads: Vec<FrozenWorkload>,
}

pub fn write(paths: &Paths, boot_id: &str, workloads: &[FrozenWorkload]) -> io::Result<()> {
    let temporary = paths.base.join("state/frozen.json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&temporary)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("frozen journal must be a regular file"));
    }
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "boot_id": boot_id, "workloads": workloads,
        }),
    )?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, paths.base.join("state/frozen.json"))?;
    File::open(paths.base.join("state"))?.sync_all()
}

pub fn read(paths: &Paths) -> io::Result<FrozenState> {
    Ok(serde_json::from_slice(&fs::read(
        paths.base.join("state/frozen.json"),
    )?)?)
}

pub fn needs_recovery(paths: &Paths) -> bool {
    match read(paths) {
        Ok(state) => !state.workloads.is_empty(),
        Err(_) => fs::metadata(paths.base.join("state/frozen.json")).is_ok_and(|m| m.len() > 0),
    }
}

pub(super) fn signal(
    platform: &impl Platform,
    id: ProcessIdentity,
    signal: Signal,
) -> io::Result<()> {
    match platform.send_signal(id, signal) {
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
            Ok(())
        }
        result => result,
    }
}

/// The caller holds the daemon directory lock throughout recovery.
/// Even a corrupt journal must not skip the independent stopped-marker sweep.
pub fn recover(
    paths: &Paths,
    platform: &mut impl Platform,
    markers: &[Marker],
    log: &mut RotatingLog,
) -> io::Result<usize> {
    let boot_id = platform.boot_id()?;
    let mut resumed = 0;
    let mut failure = None;
    let mut remaining = Vec::new();
    let saved = match read(paths) {
        Ok(state) if state.boot_id == boot_id => state.workloads,
        Ok(_) => {
            let _ = log.decision("recovery_discarded_boot", serde_json::json!({}));
            Vec::new()
        }
        Err(e) => {
            let _ = log.decision(
                "recovery_unreadable",
                serde_json::json!({"error": e.to_string()}),
            );
            Vec::new()
        }
    };
    for mut workload in saved {
        workload
            .processes
            .retain(|&id| match signal(platform, id, Signal::Continue) {
                Ok(()) => {
                    resumed += 1;
                    false
                }
                Err(e) => {
                    failure = Some(e);
                    true
                }
            });
        if !workload.processes.is_empty() {
            remaining.push(workload);
        }
    }
    match platform.list_processes(&HashSet::new(), &HashSet::new()) {
        Ok(processes) => {
            let builtins = Marker::builtins();
            for p in processes
                .into_iter()
                .filter(|p| p.stopped && p.uid == unsafe { libc::geteuid() })
            {
                let marked = platform.read_environment(p.identity).is_some_and(|env| {
                    builtins
                        .iter()
                        .chain(markers)
                        .any(|m| env.get(&m.key).is_some_and(|v| !v.is_empty()))
                });
                if marked {
                    match signal(platform, p.identity, Signal::Continue) {
                        Ok(()) => resumed += 1,
                        Err(e) => {
                            failure = Some(e);
                            remaining.push(FrozenWorkload::recovery(p.identity));
                        }
                    }
                }
            }
        }
        Err(e) => failure = Some(e),
    }
    write(paths, &boot_id, &remaining)?;
    let _ = log.decision(
        "recovery",
        serde_json::json!({"resumed": resumed, "remaining": remaining.len()}),
    );
    match failure {
        Some(e) => Err(e),
        None => Ok(resumed),
    }
}
