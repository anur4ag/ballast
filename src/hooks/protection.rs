use super::{Event, HookRequest, classify::pipelines};
use crate::attribution::AgentState;
use crate::daemon::Snapshot;
use crate::platform::{NativePlatform, Platform, Process, ProcessIdentity};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

pub type PortOwners = HashMap<u16, HashSet<ProcessIdentity>>;

#[derive(Default)]
pub struct HookEvidence {
    pub caller: Option<ProcessIdentity>,
    pub ports: Option<PortOwners>,
    pub arguments: HashMap<ProcessIdentity, Vec<String>>,
}

pub(super) fn deny(
    request: &HookRequest,
    snapshot: &Snapshot,
    evidence: &HookEvidence,
) -> Option<String> {
    if request.event != Event::PreToolUse || request.tool_name.as_deref() == Some("PowerShell") {
        return None;
    }
    let caller = evidence.caller?;
    let pipelines = pipelines(request.shell_command()?, true)?;
    let assignments: HashMap<_, _> = snapshot
        .attribution
        .processes
        .iter()
        .map(|p| (p.identity, p))
        .collect();
    let agents: HashMap<_, _> = snapshot
        .attribution
        .agents
        .iter()
        .map(|a| (a.id.as_str(), a))
        .collect();
    let mut own = BTreeSet::new();
    let mut other = BTreeSet::new();
    let mut owners = BTreeSet::new();
    for pipeline in pipelines {
        let target = match pipeline.as_slice() {
            [words] => target(words),
            [source, sink] => port_pipeline(source, sink),
            _ => None,
        };
        let Some(target) = target else { continue };
        for process in &snapshot.processes {
            let Some(attribution) = assignments.get(&process.identity) else {
                continue;
            };
            let Some(agent) = attribution
                .agent_id
                .as_deref()
                .and_then(|id| agents.get(id))
            else {
                continue;
            };
            if agent.state == AgentState::Ended {
                continue;
            }
            let own_agent = agent.root == Some(caller);
            let argv = if own_agent {
                process.argv.as_ref()
            } else {
                evidence.arguments.get(&process.identity)
            };
            if matches!(target, Target::Names { .. }) && !own_agent && argv.is_none() {
                continue;
            }
            if !target.matches(process, argv, evidence.ports.as_ref()) {
                continue;
            }
            if own_agent {
                own.insert(process.identity.pid);
            } else if agent.root.is_some()
                && agent.session_id.is_some()
                && matches!(agent.kind.as_str(), "claude" | "codex")
            {
                // An unbound/generic root may be this caller before its session marker arrives.
                other.insert(process.identity.pid);
                owners.insert(agent.id.as_str());
            }
        }
    }
    if other.is_empty() {
        return None;
    }
    let own = if own.is_empty() {
        "You have no matching PIDs in the snapshot.".to_owned()
    } else {
        format!(
            "Your own matching PIDs are {}; kill those directly.",
            own.iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Some(format!(
        "Ballast blocked this: the command would also terminate {} processes belonging to {} other agents. {own}",
        other.len(),
        owners.len()
    ))
}

fn session_caller(request: &HookRequest, snapshot: &Snapshot) -> Option<ProcessIdentity> {
    let roots: HashSet<_> = snapshot
        .attribution
        .agents
        .iter()
        .filter(|agent| {
            agent.state != AgentState::Ended
                && agent.kind == request.agent.as_str()
                && agent.session_id.as_deref() == Some(&request.session_id)
        })
        .filter_map(|agent| agent.root)
        .collect();
    (roots.len() == 1).then(|| *roots.iter().next().unwrap())
}

fn ancestry_caller(
    snapshot: &Snapshot,
    mut pid: i32,
    platform: &impl Platform,
    started: Instant,
) -> Result<Option<ProcessIdentity>, ()> {
    let mut child_start = u64::MAX;
    let mut seen = HashSet::new();
    for _ in 0..64 {
        if pid <= 0 {
            return Ok(None);
        }
        if !seen.insert(pid) || started.elapsed() >= Duration::from_millis(100) {
            return Err(());
        }
        let (identity, parent) = platform.process_parent(pid).ok_or(())?;
        if identity.start_time > child_start {
            return Err(());
        }
        if snapshot
            .processes
            .iter()
            .any(|process| process.identity == identity)
        {
            return Ok(snapshot
                .attribution
                .processes
                .iter()
                .find(|p| p.identity == identity)
                .and_then(|p| p.agent_id.as_ref())
                .and_then(|id| {
                    snapshot
                        .attribution
                        .agents
                        .iter()
                        .find(|a| a.id == *id && a.state != AgentState::Ended)
                })
                .and_then(|a| a.root));
        }
        child_start = identity.start_time;
        pid = parent;
    }
    Err(())
}

enum Target {
    Pids(Vec<i32>),
    Ports(Vec<u16>),
    Names {
        patterns: Vec<String>,
        full: bool,
        exact: bool,
        killall: bool,
    },
}
impl Target {
    fn matches(
        &self,
        process: &Process,
        argv: Option<&Vec<String>>,
        ports: Option<&PortOwners>,
    ) -> bool {
        match self {
            Self::Pids(pids) => pids.contains(&process.identity.pid),
            Self::Ports(targets) => ports.is_some_and(|ports| {
                targets.iter().any(|p| {
                    ports
                        .get(p)
                        .is_some_and(|owners| owners.contains(&process.identity))
                })
            }),
            Self::Names {
                patterns,
                full,
                exact,
                killall,
            } => {
                let text = if *full {
                    argv.map(|args| args.join(" "))
                } else if cfg!(target_os = "macos") {
                    argv.and_then(|args| args.first())
                        .and_then(|arg| arg.rsplit('/').next())
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                        .or_else(|| (!killall).then(|| process.name.clone()).flatten())
                } else {
                    process.name.clone()
                };
                if cfg!(target_os = "linux")
                    && *killall
                    && text.as_ref().is_some_and(|name| name.len() == 15)
                {
                    let Some(args) = argv else {
                        return false;
                    };
                    let name = text.as_deref().unwrap();
                    let long = args
                        .iter()
                        .map(|a| a.rsplit('/').next().unwrap_or(a))
                        .take_while(|a| !a.is_empty())
                        .find(|a| a.starts_with(name));
                    return patterns.iter().any(|pattern| {
                        long.map_or_else(|| pattern.starts_with(name), |long| pattern == long)
                    });
                }
                text.is_some_and(|text| {
                    patterns
                        .iter()
                        .any(|p| if *exact { text == *p } else { text.contains(p) })
                })
            }
        }
    }
}

fn program(words: &[String]) -> Option<(&str, &[String])> {
    let mut words = words;
    while words
        .first()
        .is_some_and(|word| super::classify::assignment(word) || word == "env" || word == "command")
    {
        words = &words[1..];
    }
    let (name, args) = words.split_first()?;
    Some((name.rsplit('/').next()?, args))
}

fn terminating(signal: &str) -> bool {
    let signal = signal.to_ascii_uppercase();
    matches!(
        signal.strip_prefix("SIG").unwrap_or(&signal),
        "TERM" | "KILL" | "INT" | "QUIT" | "HUP" | "ABRT" | "15" | "9" | "2" | "3" | "1" | "6"
    )
}

// ponytail: literal patterns only; regexes and unsupported options fail open.
// Add a regex engine if regex target resolution becomes necessary.
fn target(words: &[String]) -> Option<Target> {
    let (tool, mut args) = program(words)?;
    if tool == "fuser" {
        return fuser(args);
    }
    if !matches!(tool, "kill" | "pkill" | "killall") {
        return None;
    }
    let mut full = false;
    let mut exact = tool == "killall";
    let mut signal = "TERM";
    while let Some(arg) = args.first().filter(|arg| arg.starts_with('-')) {
        args = &args[1..];
        match arg.as_str() {
            "--" => break,
            "-f" if tool == "pkill" => full = true,
            "-x" if tool == "pkill" => exact = true,
            "-fx" | "-xf" if tool == "pkill" => {
                full = true;
                exact = true;
            }
            "-s" | "--signal"
                if tool != "pkill" && (tool != "killall" || cfg!(target_os = "linux")) =>
            {
                signal = args.first()?;
                args = &args[1..];
            }
            _ => {
                signal = arg.strip_prefix('-')?;
                if !terminating(signal) {
                    return None;
                }
            }
        }
    }
    if !terminating(signal) || args.is_empty() {
        return None;
    }
    if tool == "kill" {
        let pids: Option<Vec<i32>> = args
            .iter()
            .map(|arg| {
                arg.bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| arg.parse::<i32>().ok())
                    .flatten()
                    .filter(|&pid| pid > 0)
            })
            .collect();
        return Some(Target::Pids(pids?));
    }
    if tool == "pkill" && args.len() != 1 {
        return None;
    }
    if args.iter().any(|s| {
        s.is_empty()
            || s.starts_with('-')
            || (tool == "killall" && s.contains('/'))
            || (tool == "pkill"
                && s.contains([
                    '.', '^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\',
                ]))
    }) {
        return None;
    }
    Some(Target::Names {
        patterns: args.to_vec(),
        full,
        exact,
        killall: tool == "killall",
    })
}

fn fuser(mut args: &[String]) -> Option<Target> {
    if cfg!(target_os = "macos") {
        return None;
    } // macOS fuser has no -k.
    let mut kill = false;
    let mut tcp = false;
    let mut signal = "KILL";
    while let Some(arg) = args.first().filter(|arg| arg.starts_with('-')) {
        args = &args[1..];
        match arg.as_str() {
            "-k" => kill = true,
            "-n" if args.first().is_some_and(|a| a == "tcp") => {
                tcp = true;
                args = &args[1..];
            }
            "--" => break,
            _ => {
                signal = arg.strip_prefix('-')?;
                if !terminating(signal) {
                    return None;
                }
            }
        }
    }
    if !kill || !terminating(signal) || args.is_empty() {
        return None;
    }
    Some(Target::Ports(
        args.iter()
            .map(|arg| port(if tcp { arg } else { arg.strip_suffix("/tcp")? }))
            .collect::<Option<_>>()?,
    ))
}
fn port(text: &str) -> Option<u16> {
    text.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| text.parse().ok())
        .flatten()
        .filter(|&p| p > 0)
}
fn port_pipeline(source: &[String], sink: &[String]) -> Option<Target> {
    let ("lsof", args) = program(source)? else {
        return None;
    };
    let port_text = match args {
        [arg] => arg.strip_prefix("-ti:")?,
        [a, b] if a == "-t" => b.strip_prefix("-i:")?,
        _ => return None,
    };
    let ("xargs", mut args) = program(sink)? else {
        return None;
    };
    if args.first().is_some_and(|s| s == "-r") {
        args = &args[1..];
    }
    let ("kill", args) = program(args)? else {
        return None;
    };
    let mut check = vec!["kill".to_owned()];
    check.extend_from_slice(args);
    check.push("1".to_owned());
    let Some(Target::Pids(pids)) = target(&check) else {
        return None;
    };
    if pids != [1] {
        return None;
    }
    Some(Target::Ports(vec![port(port_text)?]))
}

fn hint_ports(request: &HookRequest) -> Option<BTreeSet<u16>> {
    if request.event != Event::PostToolUse || request.shell_command().is_none() {
        return None;
    }
    let mut texts = Vec::new();
    strings(request.tool_response.as_ref()?, &mut texts);
    let text = texts.join("\n").to_ascii_lowercase();
    if !text.contains("eaddrinuse")
        && !text.contains("address already in use")
        && !(text.contains("port ") && text.contains(" is in use"))
    {
        return None;
    }
    let mut ports = BTreeSet::new();
    for (i, _) in text.match_indices(':').chain(text.match_indices("port")) {
        let tail = &text[i..];
        let tail = if let Some(tail) = tail.strip_prefix("port") {
            tail
        } else {
            &tail[1..]
        };
        let digits = tail.trim_start_matches([' ', ':', '\'', '"', '=']);
        let end = digits.bytes().take_while(u8::is_ascii_digit).count();
        if let Some(port) = port(&digits[..end]) {
            ports.insert(port);
        }
    }
    Some(ports)
}

pub(super) fn hint(
    request: &HookRequest,
    snapshot: &Snapshot,
    evidence: &HookEvidence,
) -> Option<String> {
    let ports = hint_ports(request)?;
    let owners = evidence.ports.as_ref()?;
    let mut hints = Vec::new();
    for p in &snapshot.attribution.processes {
        let Some(agent) = p
            .agent_id
            .as_ref()
            .and_then(|id| snapshot.attribution.agents.iter().find(|a| a.id == *id))
        else {
            continue;
        };
        let Some(process) = snapshot
            .processes
            .iter()
            .find(|process| process.identity == p.identity)
        else {
            continue;
        };
        for port in ports.iter().filter(|port| {
            owners
                .get(port)
                .is_some_and(|ids| ids.contains(&p.identity))
        }) {
            let owner = if evidence.caller.is_some() && agent.root == evidence.caller {
                "your agent".to_owned()
            } else {
                format!(
                    "agent {} ({})",
                    agent.id.escape_default(),
                    agent.kind.escape_default()
                )
            };
            let advice = if agent.state == AgentState::Ended {
                match &p.workload_id {
                    Some(workload) => format!("Its agent ended. Run `ballast stop {}` to stop the leftover workload.", workload.escape_default()),
                    None => "Its agent ended. Use `ballast ps` to find the workload, then `ballast stop <workload>` to stop it.".to_owned(),
                }
            } else {
                "Start your server on a different port and use that port in your tests.".to_owned()
            };
            hints.push(format!(
                "Port {port} is held by {owner}: {} (PID {}, workspace {}). {advice}",
                process
                    .name
                    .as_deref()
                    .unwrap_or("process")
                    .escape_default(),
                p.identity.pid,
                agent.cwd.as_deref().unwrap_or("unknown").escape_default()
            ));
        }
    }
    (!hints.is_empty()).then(|| hints.join("\n"))
}
fn strings<'a>(value: &'a serde_json::Value, result: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => result.push(text),
        serde_json::Value::Array(values) => values.iter().for_each(|v| strings(v, result)),
        serde_json::Value::Object(values) => values.values().for_each(|v| strings(v, result)),
        _ => {}
    }
}

/// Runs on the bounded IPC client worker, never on the daemon tick thread.
/// Cached service ports are classification evidence, not current ownership evidence.
fn lookup_ports(request: &HookRequest, snapshot: &Snapshot) -> Option<PortOwners> {
    let ports: BTreeSet<u16> = match request.event {
        Event::PreToolUse => pipelines(request.shell_command()?, true)?
            .into_iter()
            .filter_map(|pipeline| match pipeline.as_slice() {
                [words] => target(words),
                [source, sink] => port_pipeline(source, sink),
                _ => None,
            })
            .filter_map(|target| {
                if let Target::Ports(ports) = target {
                    Some(ports)
                } else {
                    None
                }
            })
            .flatten()
            .collect(),
        Event::PostToolUse => hint_ports(request)?,
        _ => return None,
    };
    if ports.is_empty() {
        return None;
    }
    let started = Instant::now();
    let targets: Vec<_> = snapshot
        .attribution
        .processes
        .iter()
        .filter(|p| p.agent_id.is_some())
        .map(|p| p.identity)
        .collect();
    let samples = NativePlatform::new().ok()?.listening_ports_batch(&targets);
    if started.elapsed() >= Duration::from_millis(100) {
        return None;
    }
    let mut owners = PortOwners::new();
    for (id, sample) in samples {
        for port in sample.into_iter().flatten().filter(|p| ports.contains(p)) {
            owners.entry(port).or_default().insert(id);
        }
    }
    Some(owners)
}

/// Only kill and port hooks pay for native reads, on the bounded IPC client worker.
pub fn lookup(request: &HookRequest, snapshot: &Snapshot, peer: Option<i32>) -> HookEvidence {
    let started = Instant::now();
    let targets: Vec<_> = if request.event == Event::PreToolUse
        && request.tool_name.as_deref() != Some("PowerShell")
    {
        request
            .shell_command()
            .and_then(|command| pipelines(command, true))
            .into_iter()
            .flatten()
            .filter_map(|pipeline| match pipeline.as_slice() {
                [words] => target(words),
                [source, sink] => port_pipeline(source, sink),
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    if targets.is_empty() && hint_ports(request).is_none_or(|ports| ports.is_empty()) {
        return HookEvidence::default();
    }
    let platform = NativePlatform::new().ok();
    let caller = peer
        .zip(platform.as_ref())
        .ok_or(())
        .and_then(|(pid, platform)| ancestry_caller(snapshot, pid, platform, started))
        .unwrap_or_else(|()| session_caller(request, snapshot));
    if started.elapsed() >= Duration::from_millis(100)
        || (request.event == Event::PreToolUse && caller.is_none())
    {
        return HookEvidence::default();
    }
    let mut evidence = HookEvidence {
        caller,
        ports: lookup_ports(request, snapshot),
        ..Default::default()
    };
    if caller.is_some()
        && targets
            .iter()
            .any(|target| matches!(target, Target::Names { .. }))
        && let Some(platform) = platform
    {
        let other: HashSet<_> = snapshot
            .attribution
            .agents
            .iter()
            .filter(|agent| {
                agent.root.is_some()
                    && agent.root != caller
                    && agent.state != AgentState::Ended
                    && agent.session_id.is_some()
                    && matches!(agent.kind.as_str(), "claude" | "codex")
            })
            .map(|agent| &agent.id)
            .collect();
        for process in &snapshot.attribution.processes {
            if started.elapsed() >= Duration::from_millis(100) {
                return HookEvidence::default();
            }
            if process
                .agent_id
                .as_ref()
                .is_some_and(|id| other.contains(id))
                && let Some(args) = platform.read_arguments(process.identity)
            {
                evidence.arguments.insert(process.identity, args);
            }
        }
    }
    if started.elapsed() >= Duration::from_millis(100) {
        HookEvidence::default()
    } else {
        evidence
    }
}
