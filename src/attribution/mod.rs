mod display;
mod handles;
mod model;
mod registry;
mod roots;
#[cfg(test)]
mod tests;
pub use display::format_ps;
pub use handles::WorkloadHandles;
pub use model::*;
pub use registry::{Marker, MarkerLevel};
use registry::{agent_kind, binary, binary_candidate_kind, builtin_shells};

use crate::platform::{Environment, Platform, Process, ProcessIdentity, ProcessLiveness};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

struct Cached {
    // Resolution rules: identity observations bind markers and preserve omitted ancestry.
    uid: u32,
    exe: Option<String>,
    ppid: i32,
    pgid: i32,
    // Workload assignment: an observed detachment keeps its previous membership.
    detached: bool,
    // Workload assignment: a positively observed tool-call boundary survives exec and read gaps.
    workload_root: bool,
    // Root validation: established roots no longer depend on a live marker claimant.
    root_rule: Option<roots::Rule>,
    // Attribution algorithm: sample environment once per identity/executable observation.
    env: Option<Environment>,
    attribution: ProcessAttribution,
    // Workload class and growth rules: retain successful age/port samples and their clocks.
    seen: Instant,
    age: Option<Duration>,
    next_age: Instant,
}
#[derive(Default)]
struct History(VecDeque<(Instant, u64)>);
impl History {
    fn sample(&mut self, now: Instant, summary: &mut MemorySummary, discard: bool) {
        if discard || !summary.complete {
            self.0.clear();
        }
        summary.growth_30s_bytes = None;
        if !summary.complete {
            return;
        }
        self.0.push_back((now, summary.bytes));
        while self.0.get(1).is_some_and(|(then, _)| {
            now.saturating_duration_since(*then) >= Duration::from_secs(30)
        }) {
            self.0.pop_front();
        }
        if let Some(&(then, bytes)) = self.0.front() {
            if now.duration_since(then) >= Duration::from_secs(30) {
                summary.growth_30s_bytes = Some(
                    (i128::from(summary.bytes) - i128::from(bytes))
                        .clamp(i64::MIN as i128, i64::MAX as i128) as i64,
                );
            }
        }
    }
}
pub struct Attributor {
    uid: u32,
    markers: Vec<Marker>,
    shells: Vec<String>,
    cache: HashMap<ProcessIdentity, Cached>,
    agents: BTreeMap<String, Agent>,
    workloads: BTreeMap<String, Workload>,
    owners: BTreeMap<String, Owner>,
    histories: HashMap<String, History>,
    next_cwd: HashMap<String, Instant>,
    services: HashSet<String>,
    next_ports: Option<Instant>,
    claims: HashMap<(ProcessIdentity, roots::Rule), roots::Claim>,
    path_only: HashSet<ProcessIdentity>,
}
impl Attributor {
    pub fn new(additions: Vec<Marker>, additional_shells: Vec<String>) -> Self {
        let mut markers = Marker::builtins();
        markers.extend(additions);
        let mut shells = builtin_shells();
        shells.extend(additional_shells);
        Self {
            uid: unsafe { libc::geteuid() },
            markers,
            shells,
            cache: HashMap::new(),
            agents: BTreeMap::new(),
            workloads: BTreeMap::new(),
            owners: BTreeMap::new(),
            histories: HashMap::new(),
            next_cwd: HashMap::new(),
            services: HashSet::new(),
            next_ports: None,
            claims: HashMap::new(),
            path_only: HashSet::new(),
        }
    }
    pub fn reset_growth(&mut self) {
        self.histories.clear();
    }
    pub fn metric_targets(&self) -> HashSet<ProcessIdentity> {
        self.cache
            .iter()
            .filter(|(_, c)| c.attribution.agent_id.is_some())
            .map(|(id, _)| *id)
            .collect()
    }
    pub fn watched(&self, frozen: &HashSet<ProcessIdentity>) -> HashSet<ProcessIdentity> {
        let mut set = self.metric_targets();
        set.extend(self.agents.values().filter_map(|a| a.root));
        set.extend(self.claims.values().flat_map(|c| c.watched()));
        set.retain(|id| self.cache.contains_key(id));
        set.extend(frozen);
        set
    }
    pub fn update(
        &mut self,
        platform: &impl Platform,
        processes: &mut [Process],
        now: Instant,
        wall_ms: u64,
        discard: bool,
    ) -> AttributionSnapshot {
        let observed: HashSet<_> = processes.iter().map(|p| p.identity).collect();
        let by_pid: HashMap<i32, &Process> =
            processes.iter().map(|p| (p.identity.pid, p)).collect();
        let removed: HashSet<_> = self
            .cache
            .keys()
            .copied()
            .filter(|id| {
                by_pid.get(&id.pid).map_or_else(
                    || platform.process_liveness(*id) == ProcessLiveness::Gone,
                    |p| p.identity != *id,
                )
            })
            .collect();
        self.cache.retain(|id, _| !removed.contains(id));
        for p in processes.iter() {
            if self
                .cache
                .get(&p.identity)
                .is_none_or(|c| c.exe != p.exe || c.uid != p.uid)
            {
                let env = (p.uid == self.uid)
                    .then(|| platform.read_environment(p.identity))
                    .flatten()
                    .map(|mut env| {
                        env.retain(|key, _| {
                            self.markers.iter().any(|m| {
                                &m.key == key
                                    || m.name_key.as_ref() == Some(key)
                                    || m.root_pid_key.as_ref() == Some(key)
                            })
                        });
                        env
                    });
                let old = self.cache.remove(&p.identity);
                let detached =
                    old.as_ref().is_some_and(|c| c.detached || c.ppid != p.ppid) || p.ppid == 1;
                let (seen, age, next_age) = old
                    .as_ref()
                    .map_or((now, None, now), |c| (c.seen, c.age, c.next_age));
                self.cache.insert(
                    p.identity,
                    Cached {
                        uid: p.uid,
                        exe: p.exe.clone(),
                        ppid: p.ppid,
                        pgid: p.pgid,
                        detached,
                        workload_root: old.as_ref().is_some_and(|c| c.workload_root),
                        root_rule: old.as_ref().and_then(|c| c.root_rule),
                        attribution: old.map_or(
                            ProcessAttribution {
                                identity: p.identity,
                                owner_id: None,
                                agent_id: None,
                                workload_id: None,
                                role: ProcessRole::Unattributed,
                                environment_known: env.is_some(),
                                listening_ports: None,
                                ports_sampled_at_ms: None,
                            },
                            |mut c| {
                                c.attribution.environment_known = env.is_some();
                                c.attribution.listening_ports = None;
                                c.attribution.ports_sampled_at_ms = None;
                                c.attribution
                            },
                        ),
                        env,
                        seen,
                        age,
                        next_age,
                    },
                );
            }
            let Some(cached) = self.cache.get_mut(&p.identity) else {
                continue;
            };
            cached.detached |= cached.ppid != p.ppid || p.ppid == 1;
            cached.ppid = p.ppid;
            cached.pgid = p.pgid;
        }
        // Omission preserves identity and the last observed parent, not executable readability.
        let omitted: Vec<_> = self
            .cache
            .iter()
            .filter(|(id, _)| !observed.contains(id))
            .map(|(&identity, cached)| Process {
                identity,
                ppid: cached.ppid,
                pgid: cached.pgid,
                uid: cached.uid,
                stopped: false,
                name: None,
                exe: None,
                argv: None,
                metrics: None,
            })
            .collect();
        let by_pid: HashMap<_, _> = processes
            .iter()
            .chain(&omitted)
            .map(|p| (p.identity.pid, p))
            .collect();
        let discovery = self.discover_roots(platform, &by_pid, wall_ms);
        let mut contexts = HashMap::new();
        let mut visiting = HashSet::new();
        let mut ordered: Vec<_> = by_pid.values().copied().collect();
        ordered.sort_by_key(|p| p.identity);
        for p in ordered {
            self.resolve(
                p,
                &by_pid,
                &discovery,
                &mut contexts,
                &mut visiting,
                wall_ms,
            );
        }
        for a in self.agents.values_mut() {
            let next = self.next_cwd.entry(a.id.clone()).or_insert(now);
            if now >= *next {
                a.cwd = a
                    .root
                    .filter(|root| self.cache.get(root).is_some_and(|c| c.uid == self.uid))
                    .and_then(|root| platform.process_cwd(root))
                    .map(|p| p.to_string_lossy().into_owned());
                *next = now + Duration::from_secs(5);
            }
            a.memory = MemorySummary {
                complete: true,
                ..Default::default()
            };
        }
        drop(by_pid);
        for w in self.workloads.values_mut() {
            w.memory = MemorySummary {
                complete: true,
                ..Default::default()
            };
        }
        let ports_due = self.next_ports.is_none_or(|next| now >= next);
        let due: Vec<_> = self
            .cache
            .iter()
            .filter(|(_, c)| {
                ports_due
                    && c.attribution
                        .workload_id
                        .as_ref()
                        .is_some_and(|id| !self.services.contains(id))
            })
            .map(|(&id, _)| id)
            .collect();
        let mut port_samples = platform.listening_ports_batch(&due);
        if ports_due {
            self.next_ports = Some(now + Duration::from_secs(5));
        }
        let mut live_agents = HashSet::new();
        let mut live_workloads = HashSet::new();
        let mut unknown_ports = HashSet::new();
        for p in processes.iter_mut() {
            let Some(c) = self.cache.get_mut(&p.identity) else {
                continue;
            };
            let a = &mut c.attribution;
            let Some(agent) = a.agent_id.as_ref() else {
                continue;
            };
            live_agents.insert(agent.clone());
            if c.age.is_none() && now >= c.next_age {
                c.age = platform.process_age(p.identity);
                c.next_age = now + Duration::from_secs(5);
                c.seen = now;
            }
            if p.metrics.is_none() {
                p.metrics = platform.process_metrics(p.identity);
            }
            if let Some(ports) = port_samples.remove(&p.identity).flatten() {
                a.listening_ports = Some(ports);
                a.ports_sampled_at_ms = Some(wall_ms);
            }
            if let Some(agent) = self.agents.get_mut(agent) {
                add_memory(&mut agent.memory, p);
            }
            if let Some(workload) = &a.workload_id {
                live_workloads.insert(workload.clone());
                if c.age.is_some_and(|age| {
                    age.saturating_add(now.duration_since(c.seen)) > Duration::from_secs(600)
                }) {
                    self.services.insert(workload.clone());
                }
                if a.listening_ports.is_none() || c.age.is_none() {
                    unknown_ports.insert(workload.clone());
                }
                if let Some(workload) = self.workloads.get_mut(workload) {
                    add_memory(&mut workload.memory, p);
                }
                if a.listening_ports
                    .as_ref()
                    .is_some_and(|ports| !ports.is_empty())
                {
                    self.services.insert(workload.clone());
                }
            }
        }
        for (id, c) in &self.cache {
            if observed.contains(id) {
                continue;
            }
            if let Some(agent) = &c.attribution.agent_id {
                live_agents.insert(agent.clone());
                if let Some(agent) = self.agents.get_mut(agent) {
                    agent.memory.complete = false;
                }
            }
            if let Some(workload) = &c.attribution.workload_id {
                live_workloads.insert(workload.clone());
                unknown_ports.insert(workload.clone());
                if let Some(workload) = self.workloads.get_mut(workload) {
                    workload.memory.complete = false;
                }
            }
        }
        // A live workload root preserves its record even while membership is provisional.
        for w in self.workloads.values_mut() {
            if self.cache.contains_key(&w.root) && !live_workloads.contains(&w.id) {
                live_workloads.insert(w.id.clone());
                live_agents.insert(w.agent_id.clone());
                unknown_ports.insert(w.id.clone());
                w.memory.complete = false;
            }
        }
        self.agents.retain(|id, _| live_agents.contains(id));
        self.workloads.retain(|id, _| live_workloads.contains(id));
        self.next_cwd.retain(|id, _| live_agents.contains(id));
        self.prune_claims();
        self.services.retain(|id| live_workloads.contains(id));
        self.histories
            .retain(|id, _| live_agents.contains(id) || live_workloads.contains(id));
        for a in self.agents.values_mut() {
            self.histories
                .entry(a.id.clone())
                .or_default()
                .sample(now, &mut a.memory, discard);
        }
        for w in self.workloads.values_mut() {
            w.class = if self.services.contains(&w.id) || unknown_ports.contains(&w.id) {
                WorkloadClass::Service
            } else {
                WorkloadClass::Batch
            };
            self.histories
                .entry(w.id.clone())
                .or_default()
                .sample(now, &mut w.memory, discard);
        }
        let live_owners: HashSet<_> = self
            .cache
            .values()
            .filter_map(|c| c.attribution.owner_id.as_ref())
            .collect();
        self.owners.retain(|id, _| live_owners.contains(id));
        AttributionSnapshot {
            owners: self.owners.values().cloned().collect(),
            agents: self.agents.values().cloned().collect(),
            workloads: self.workloads.values().cloned().collect(),
            processes: processes
                .iter()
                .filter_map(|p| self.cache.get(&p.identity).map(|c| c.attribution.clone()))
                .collect(),
        }
    }
    fn owner_marker(&self, id: ProcessIdentity) -> Option<Owner> {
        let env = self.cache.get(&id)?.env.as_ref()?;
        self.markers
            .iter()
            .filter(|m| m.level == MarkerLevel::Owner)
            .find_map(|m| {
                env.get(&m.key).filter(|v| !v.is_empty()).map(|id| Owner {
                    id: id.clone(),
                    name: m.name_key.as_ref().and_then(|key| env.get(key)).cloned(),
                })
            })
    }
    fn ensure_agent(
        &mut self,
        id: String,
        root: Option<&Process>,
        kind: &str,
        session_id: Option<String>,
        owner_id: Option<String>,
        wall_ms: u64,
    ) {
        let a = self.agents.entry(id.clone()).or_insert_with(|| Agent {
            id,
            owner_id: owner_id.clone(),
            session_id: session_id.clone(),
            kind: kind.into(),
            root: root.map(|p| p.identity),
            cwd: None,
            state: if root.is_some() {
                AgentState::Unknown
            } else {
                AgentState::Ended
            },
            ended_at_ms: root.is_none().then_some(wall_ms),
            memory: MemorySummary::default(),
        });
        let can_upgrade = a.kind == "generic" || a.session_id.is_none();
        if kind != "generic" && can_upgrade {
            if a.kind == "generic" {
                a.session_id = None;
            }
            a.kind = kind.into();
        }
        if session_id.is_some() && can_upgrade {
            a.session_id = session_id;
        }
        if owner_id.is_some() {
            a.owner_id = owner_id;
        }
    }
    fn resolve(
        &mut self,
        p: &Process,
        by_pid: &HashMap<i32, &Process>,
        discovery: &roots::Discovery,
        contexts: &mut HashMap<ProcessIdentity, Option<ProcessAttribution>>,
        visiting: &mut HashSet<ProcessIdentity>,
        wall_ms: u64,
    ) {
        let roots::Discovery {
            roots,
            marked,
            nearest,
        } = discovery;
        if contexts.contains_key(&p.identity) || !visiting.insert(p.identity) {
            return;
        }
        if p.uid != self.uid {
            let Some(cached) = self.cache.get_mut(&p.identity) else {
                return;
            };
            let a = &mut cached.attribution;
            a.owner_id = None;
            a.agent_id = None;
            a.workload_id = None;
            a.role = ProcessRole::Unattributed;
            a.listening_ports = None;
            a.ports_sampled_at_ms = None;
            contexts.insert(p.identity, None);
            visiting.remove(&p.identity);
            return;
        }
        let parent = by_pid.get(&p.ppid).copied().filter(|parent| {
            parent.uid == p.uid
                && parent.identity.start_time <= p.identity.start_time
                && !visiting.contains(&parent.identity)
        });
        if let Some(parent) = parent {
            self.resolve(parent, by_pid, discovery, contexts, visiting, wall_ms);
        }
        let inherited = parent.and_then(|parent| contexts.get(&parent.identity).cloned().flatten());
        let Some(old) = self.cache.get(&p.identity).map(|c| c.attribution.clone()) else {
            return;
        };
        let mut owner = self
            .owner_marker(p.identity)
            .map(|o| o.id)
            .or_else(|| inherited.as_ref().and_then(|a| a.owner_id.clone()))
            .or(old.owner_id.clone());
        let root = roots
            .get(&p.identity)
            .filter(|id| self.agents.contains_key(*id));
        let agent = root
            .cloned()
            .or_else(|| marked.get(&p.identity).cloned())
            .or_else(|| nearest.get(&p.identity).cloned().flatten())
            .or_else(|| inherited.as_ref().and_then(|a| a.agent_id.clone()))
            .or(old
                .agent_id
                .clone()
                .filter(|id| self.agents.contains_key(id)))
            .filter(|id| self.agents.contains_key(id));
        if owner.is_none() {
            owner = agent
                .as_ref()
                .and_then(|id| self.agents.get(id))
                .and_then(|a| a.owner_id.clone());
        }
        let observed_workload_root = inherited.as_ref().is_some_and(|parent| {
            root.is_none()
                && parent.agent_id == agent
                && parent.role == ProcessRole::AgentRoot
                && parent.identity.pid == p.ppid
                && tool_shell(p, &self.shells)
        });
        let starts_workload = if let Some(cached) = self.cache.get_mut(&p.identity) {
            cached.workload_root |= observed_workload_root;
            cached.workload_root
        } else {
            false
        };
        let mut workload = None;
        let role = if let Some(root) = root {
            if self
                .agents
                .get(root)
                .is_none_or(|a| a.root.is_none() && a.state == AgentState::Unknown)
            {
                ProcessRole::AgentInternal
            } else {
                ProcessRole::AgentRoot
            }
        } else if let Some(agent) = &agent {
            if self
                .agents
                .get(agent)
                .is_none_or(|a| a.root.is_none() && a.state == AgentState::Unknown)
            {
                ProcessRole::AgentInternal
            } else if self.cache.get(&p.identity).is_some_and(|c| c.detached)
                && inherited
                    .as_ref()
                    .is_none_or(|parent| parent.agent_id.is_none())
                && old.agent_id.as_ref() == Some(agent)
                && matches!(old.role, ProcessRole::Workload | ProcessRole::AgentInternal)
            {
                workload = old.workload_id;
                old.role
            } else if let Some(parent) = inherited
                .as_ref()
                .filter(|a| a.agent_id.as_ref() == Some(agent))
            {
                if starts_workload {
                    workload = Some(self.new_workload(p, agent, None, wall_ms));
                    ProcessRole::Workload
                } else if parent.role == ProcessRole::Workload {
                    workload = parent.workload_id.clone();
                    ProcessRole::Workload
                } else {
                    ProcessRole::AgentInternal
                }
            } else if p.ppid == 1 {
                workload = Some(self.new_workload(p, agent, Some(p.pgid), wall_ms));
                ProcessRole::Workload
            } else {
                ProcessRole::AgentInternal
            }
        } else {
            ProcessRole::Unattributed
        };
        if let Some(a) = agent.as_ref().and_then(|id| self.agents.get_mut(id)) {
            if a.owner_id.is_none() {
                a.owner_id.clone_from(&owner);
            }
        }
        let Some(cached) = self.cache.get_mut(&p.identity) else {
            return;
        };
        let a = &mut cached.attribution;
        a.owner_id = owner;
        a.agent_id = agent;
        a.workload_id = workload;
        a.role = role;
        let context = if self.path_only.contains(&p.identity) {
            inherited
        } else {
            Some(a.clone())
        };
        contexts.insert(p.identity, context);
        visiting.remove(&p.identity);
    }
    fn new_workload(
        &mut self,
        p: &Process,
        agent: &str,
        pgid: Option<i32>,
        wall_ms: u64,
    ) -> String {
        if let Some(pgid) = pgid {
            if let Some(w) = self
                .workloads
                .values()
                .find(|w| w.agent_id == agent && w.detached_pgid == Some(pgid))
            {
                return w.id.clone();
            }
        }
        let id = format!("w:{agent}:{}:{}", p.identity.pid, p.identity.start_time);
        self.workloads
            .entry(id.clone())
            .or_insert_with(|| Workload {
                id: id.clone(),
                agent_id: agent.into(),
                root: p.identity,
                label: p
                    .argv
                    .as_ref()
                    .map(|a| a.join(" "))
                    .or_else(|| p.exe.clone())
                    .unwrap_or_else(|| format!("pid {}", p.identity.pid)),
                class: WorkloadClass::Batch,
                first_seen_ms: wall_ms,
                detached_pgid: pgid,
                memory: MemorySummary::default(),
            });
        id
    }
}
fn agent_id(id: ProcessIdentity) -> String {
    format!("a:{}:{}", id.pid, id.start_time)
}
fn tool_shell(p: &Process, shells: &[String]) -> bool {
    let basename = |path: &str| {
        std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| shells.iter().any(|shell| shell == name))
    };
    let argv = p.argv.as_deref().unwrap_or_default();
    (p.exe.as_deref().is_some_and(basename) || argv.first().is_some_and(|arg| basename(arg)))
        && argv
            .iter()
            .skip(1)
            .take_while(|arg| arg.starts_with('-'))
            .any(|arg| !arg.starts_with("--") && arg.contains('c'))
}
fn ancestors<'a>(p: &Process, table: &HashMap<i32, &'a Process>) -> Vec<&'a Process> {
    let mut result: Vec<&Process> = Vec::new();
    let mut pid = p.ppid;
    while let Some(parent) = table.get(&pid) {
        if parent.uid != p.uid
            || parent.identity.start_time > p.identity.start_time
            || parent.identity == p.identity
            || result.iter().any(|p| p.identity == parent.identity)
        {
            break;
        }
        result.push(*parent);
        pid = parent.ppid;
    }
    result
}
fn add_memory(summary: &mut MemorySummary, p: &Process) {
    if let Some(metrics) = p.metrics {
        summary.bytes = summary.bytes.saturating_add(metrics.memory_bytes);
    } else {
        summary.complete = false;
    }
}
