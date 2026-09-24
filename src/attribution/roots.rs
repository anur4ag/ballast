use super::*;
use std::collections::hash_map::Entry;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) enum Rule {
    Binary,
    Owner,
    Marker(usize),
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Validity {
    Valid,
    Invalid,
    Unknown,
}
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Candidate {
    pid: i32,
    identity: Option<ProcessIdentity>,
    validity: Validity,
    identity_rejected: bool,
}
#[derive(Clone)]
pub(super) struct Claim {
    claimant: ProcessIdentity,
    uid: u32,
    rule: Rule,
    kind: String,
    session: Option<String>,
    candidates: Vec<Candidate>,
    agent: Option<String>,
}
impl Claim {
    #[cfg(test)]
    pub(super) fn has_unknown_candidate(&self) -> bool {
        self.candidates
            .iter()
            .any(|candidate| candidate.validity == Validity::Unknown)
    }

    fn priority(&self, markers: &[Marker]) -> u8 {
        match self.rule {
            Rule::Marker(i) if markers.get(i).is_some_and(|m| m.root_pid_key.is_some()) => 0,
            Rule::Marker(_) => 1,
            Rule::Binary => 2,
            Rule::Owner => 3,
        }
    }

    fn selected(&self) -> Option<&Candidate> {
        self.candidates
            .iter()
            .find(|c| c.validity != Validity::Invalid)
    }
    fn rejects_root(&self, root: ProcessIdentity) -> bool {
        self.candidates
            .iter()
            .any(|c| c.pid == root.pid && c.validity == Validity::Invalid)
    }
    pub(super) fn watched(&self) -> impl Iterator<Item = ProcessIdentity> + '_ {
        self.candidates.iter().filter_map(|c| c.identity)
    }
    pub(super) fn retained(
        &self,
        cache: &HashMap<ProcessIdentity, Cached>,
        agents: &BTreeMap<String, Agent>,
    ) -> bool {
        cache.contains_key(&self.claimant)
            || self
                .agent
                .as_ref()
                .and_then(|id| agents.get(id))
                .is_some_and(|a| a.root.is_none() && a.state == AgentState::Unknown)
    }
}

pub(super) struct Discovery {
    pub roots: HashMap<ProcessIdentity, String>,
    pub marked: HashMap<ProcessIdentity, String>,
    pub nearest: HashMap<ProcessIdentity, Option<String>>,
}

impl Attributor {
    pub(super) fn discover_roots(
        &mut self,
        platform: &impl Platform,
        by_pid: &HashMap<i32, &Process>,
        wall_ms: u64,
    ) -> Discovery {
        let previously_provisional: HashSet<_> = self
            .agents
            .values()
            .filter(|a| a.root.is_none() && a.state == AgentState::Unknown)
            .map(|a| a.id.clone())
            .collect();
        let uid = self.uid;
        for p in by_pid.values().copied().filter(|p| p.uid == uid) {
            let owner = self.owner_marker(p.identity);
            if let Some(owner) = &owner {
                self.owners.insert(owner.id.clone(), owner.clone());
            }
            let Some(cached) = self.cache.get(&p.identity) else {
                continue;
            };
            let agent_marker = cached.env.as_ref().is_some_and(|env| {
                self.markers.iter().any(|m| {
                    m.level == MarkerLevel::Agent && env.get(&m.key).is_some_and(|v| !v.is_empty())
                })
            });
            if let Some(kind) = binary_candidate_kind(p) {
                self.record_claim(p, Rule::Binary, kind.into(), None, vec![p.identity.pid]);
            } else if !agent_marker
                && (cached.attribution.agent_id.is_none()
                    || self.claims.contains_key(&(p.identity, Rule::Owner)))
                && owner.as_ref().is_some_and(|o| {
                    !ancestors(p, by_pid).iter().any(|a| {
                        self.owner_marker(a.identity)
                            .is_some_and(|parent| parent.id == o.id)
                    })
                })
            {
                self.record_claim(
                    p,
                    Rule::Owner,
                    "generic".into(),
                    owner.map(|o| o.id),
                    vec![p.identity.pid],
                );
            }
            let Some(env) = self.cache.get(&p.identity).and_then(|c| c.env.clone()) else {
                continue;
            };
            for index in 0..self.markers.len() {
                let Some(marker) = self.markers.get(index) else {
                    continue;
                };
                if marker.level != MarkerLevel::Agent {
                    continue;
                }
                let Some(value) = env.get(&marker.key).filter(|v| !v.is_empty()) else {
                    continue;
                };
                let pids = if let Some(key) = &marker.root_pid_key {
                    env.get(key)
                        .and_then(|v| v.parse::<i32>().ok())
                        .filter(|pid| *pid > 0)
                        .into_iter()
                        .collect()
                } else {
                    let lineage: Vec<_> = std::iter::once(p).chain(ancestors(p, by_pid)).collect();
                    let mut pids: Vec<_> = lineage.iter().map(|p| p.identity.pid).collect();
                    if let Some(last) = lineage.last() {
                        if last.ppid > 1 && !by_pid.contains_key(&last.ppid) {
                            pids.push(last.ppid);
                        }
                    }
                    pids
                };
                self.record_claim(
                    p,
                    Rule::Marker(index),
                    marker.kind.clone().unwrap_or_else(|| "generic".into()),
                    marker.session_id.then(|| value.clone()),
                    pids,
                );
            }
        }
        let mut claims: Vec<_> = std::mem::take(&mut self.claims).into_iter().collect();
        for (_, claim) in &mut claims {
            for candidate in &mut claim.candidates {
                let next = validate(
                    candidate,
                    claim.claimant,
                    claim.uid,
                    claim.rule,
                    &self.markers,
                    platform,
                    by_pid,
                );
                let stale_carrier = if let Rule::Marker(index) = claim.rule {
                    self.markers
                        .get(index)
                        .and_then(|m| m.root_pid_key.as_ref().map(|key| (m, key)))
                        .is_some_and(|(marker, key)| {
                            by_pid
                                .get(&claim.claimant.pid)
                                .filter(|p| p.identity == claim.claimant)
                                .zip(by_pid.get(&candidate.pid))
                                .is_some_and(|(claimant, root)| {
                                    ancestors(claimant, by_pid)
                                        .into_iter()
                                        .take_while(|p| p.identity != root.identity)
                                        .any(|p| {
                                            p.identity.start_time < root.identity.start_time
                                                && self
                                                    .cache
                                                    .get(&p.identity)
                                                    .and_then(|c| c.env.as_ref())
                                                    .is_some_and(|env| {
                                                        env.get(&marker.key)
                                                            .is_some_and(|v| !v.is_empty())
                                                            && env
                                                                .get(key)
                                                                .and_then(|v| v.parse::<i32>().ok())
                                                                == Some(candidate.pid)
                                                    })
                                        })
                                })
                        })
                } else {
                    false
                };
                candidate.validity = if stale_carrier {
                    Validity::Invalid
                } else {
                    next
                };
            }
        }
        // Specificity decides metadata; attached claims break ties before detached claims.
        claims.sort_by_key(|(_, claim)| {
            let distance = claim
                .selected()
                .and_then(|c| c.identity)
                .and_then(|root| {
                    by_pid.get(&claim.claimant.pid).and_then(|p| {
                        std::iter::once(*p)
                            .chain(ancestors(p, by_pid))
                            .position(|p| p.identity == root)
                    })
                })
                .unwrap_or(usize::MAX);
            (
                claim.priority(&self.markers),
                distance,
                claim.claimant,
                claim.rule,
            )
        });
        let mut roots = HashMap::new();
        let mut valid_agents = HashMap::new();
        for (&identity, cached) in &mut self.cache {
            if cached.uid != uid {
                cached.root_rule = None;
            }
            let Some(rule) = cached.root_rule else {
                continue;
            };
            let mut candidate = Candidate {
                pid: identity.pid,
                identity: Some(identity),
                validity: Validity::Valid,
                identity_rejected: false,
            };
            if validate(
                &mut candidate,
                identity,
                cached.uid,
                rule,
                &self.markers,
                platform,
                by_pid,
            ) == Validity::Valid
            {
                let id = agent_id(identity);
                roots.insert(identity, id.clone());
                valid_agents.insert(id, identity);
            } else {
                cached.root_rule = None;
            }
        }
        for (_, claim) in &mut claims {
            let Some(candidate) = claim.selected().filter(|c| c.validity == Validity::Valid) else {
                continue;
            };
            let Some(identity) = candidate.identity else {
                continue;
            };
            let id = agent_id(identity);
            let conflicting_parent = by_pid.get(&claim.claimant.pid).is_some_and(|p| {
                std::iter::once(*p).chain(ancestors(p, by_pid)).any(|p| {
                    self.cache
                        .get(&p.identity)
                        .and_then(|c| c.attribution.agent_id.as_ref())
                        .and_then(|id| self.agents.get(id))
                        .is_some_and(|a| {
                            a.kind == claim.kind
                                && claim.session.is_some()
                                && a.session_id == claim.session
                                && a.id.starts_with("a:")
                                && a.id != id
                        })
                })
            });
            if conflicting_parent {
                for c in &mut claim.candidates {
                    c.validity = Validity::Invalid;
                }
                continue;
            }
            let owner = self.owner_marker(identity).map(|o| o.id);
            if claim.rule != Rule::Owner || self.agents.get(&id).is_none_or(|a| a.kind == "generic")
            {
                self.ensure_agent(
                    id.clone(),
                    by_pid.get(&identity.pid).copied(),
                    &claim.kind,
                    claim.session.clone(),
                    owner,
                    wall_ms,
                );
            }
            if let Some(cached) = self.cache.get_mut(&identity) {
                if cached.root_rule.is_none() || cached.root_rule == Some(Rule::Owner) {
                    cached.root_rule = Some(claim.rule);
                }
            }
            roots.insert(identity, id.clone());
            valid_agents.insert(id.clone(), identity);
            claim.agent = Some(id);
        }
        let mut provisional = HashSet::new();
        self.path_only.clear();
        for (_, claim) in &mut claims {
            let Some(candidate) = claim.selected().filter(|c| c.validity == Validity::Unknown)
            else {
                continue;
            };
            let identity = candidate.identity;
            let fallback = claim
                .session
                .as_ref()
                .map(|s| format!("session:{}:{s}", claim.kind))
                .unwrap_or_else(|| {
                    format!(
                        "pending:{}:{}",
                        claim.claimant.pid, claim.claimant.start_time
                    )
                });
            let fallback = if self.agents.values().any(|a| {
                valid_agents.contains_key(&a.id)
                    && a.kind == claim.kind
                    && a.session_id == claim.session
            }) {
                format!(
                    "pending:{}:{}",
                    candidate.pid,
                    candidate.identity.map_or(0, |i| i.start_time)
                )
            } else {
                fallback
            };
            let id = identity
                .filter(|identity| !self.path_only.contains(identity))
                .and_then(|identity| roots.get(&identity))
                .filter(|id| !valid_agents.contains_key(*id))
                .cloned()
                .unwrap_or(fallback);
            let owner = self.owner_marker(claim.claimant).map(|o| o.id);
            self.ensure_agent(
                id.clone(),
                None,
                &claim.kind,
                claim.session.clone(),
                owner,
                wall_ms,
            );
            provisional.insert(id.clone());
            let explicit = matches!(claim.rule, Rule::Binary)
                || matches!(claim.rule, Rule::Marker(i) if self.markers.get(i).is_some_and(|m| m.root_pid_key.is_some()));
            if explicit {
                let identity = identity.unwrap_or(ProcessIdentity {
                    pid: candidate.pid,
                    start_time: 0,
                });
                if !valid_agents.values().any(|root| *root == identity) {
                    roots.insert(identity, id.clone());
                    self.path_only.remove(&identity);
                }
            } else if let Some(identity) = identity {
                if let Entry::Vacant(entry) = roots.entry(identity) {
                    entry.insert(id.clone());
                    self.path_only.insert(identity);
                }
            }
            if !explicit {
                if let Some(p) = by_pid.get(&claim.claimant.pid) {
                    for ancestor in ancestors(p, by_pid) {
                        if valid_agents.values().any(|root| *root == ancestor.identity) {
                            break;
                        }
                        if claim.candidates.iter().any(|c| {
                            c.identity == Some(ancestor.identity) && c.validity == Validity::Unknown
                        }) && !roots.contains_key(&ancestor.identity)
                        {
                            roots.insert(ancestor.identity, id.clone());
                            self.path_only.insert(ancestor.identity);
                        }
                    }
                }
            }
            roots.entry(claim.claimant).or_insert_with(|| id.clone());
            self.path_only.remove(&claim.claimant);
            claim.agent = Some(id);
        }
        let mut nearest = HashMap::new();
        let mut missing: HashMap<i32, (ProcessIdentity, String)> = HashMap::new();
        for (id, agent) in roots.iter().filter(|(id, _)| !self.path_only.contains(id)) {
            if missing
                .get(&id.pid)
                .is_none_or(|(old, _)| old.start_time < id.start_time)
            {
                missing.insert(id.pid, (*id, agent.clone()));
            }
        }
        let mut visiting = HashSet::new();
        for p in by_pid.values().filter(|p| p.uid == uid) {
            inherited_root(
                p,
                by_pid,
                &roots,
                &missing,
                &self.path_only,
                &mut nearest,
                &mut visiting,
            );
        }
        for (identity, agent) in &roots {
            nearest.insert(*identity, Some(agent.clone()));
        }
        let mut marked = HashMap::new();
        for (_, claim) in &mut claims {
            let Some(p) = by_pid
                .get(&claim.claimant.pid)
                .filter(|p| p.identity == claim.claimant)
            else {
                continue;
            };
            let nearest = nearest.get(&p.identity).and_then(|id| id.as_ref());
            if let Some(nearest) = nearest {
                if claim
                    .selected()
                    .and_then(|c| c.identity)
                    .and_then(|root| roots.get(&root))
                    != Some(nearest)
                {
                    continue;
                }
                claim.agent = Some(nearest.clone());
            } else if claim.selected().is_none() {
                let Some(session) = &claim.session else {
                    continue;
                };
                let id = self
                    .agents
                    .values()
                    .filter(|a| a.kind == claim.kind && a.session_id.as_ref() == Some(session))
                    .filter(|a| {
                        valid_agents
                            .get(&a.id)
                            .is_none_or(|root| !claim.rejects_root(*root))
                    })
                    .min_by_key(|a| !valid_agents.contains_key(&a.id))
                    .map(|a| a.id.clone())
                    .unwrap_or_else(|| format!("session:{}:{session}", claim.kind));
                let owner = self.owner_marker(claim.claimant).map(|o| o.id);
                self.ensure_agent(
                    id.clone(),
                    None,
                    &claim.kind,
                    claim.session.clone(),
                    owner,
                    wall_ms,
                );
                claim.agent = Some(id);
            }
            if let Some(id) = &claim.agent {
                marked.entry(claim.claimant).or_insert(id.clone());
            }
        }
        let reconciled: HashMap<_, _> = self
            .agents
            .values()
            .filter(|a| {
                !a.id.starts_with("a:")
                    && !valid_agents.contains_key(&a.id)
                    && !provisional.contains(&a.id)
            })
            .filter_map(|orphan| {
                self.agents
                    .values()
                    .find(|a| {
                        valid_agents.contains_key(&a.id)
                            && a.kind == orphan.kind
                            && a.session_id.is_some()
                            && a.session_id == orphan.session_id
                            && valid_agents.get(&a.id).is_some_and(|root| {
                                !claims.iter().any(|(_, claim)| {
                                    claim.agent.as_ref() == Some(&orphan.id)
                                        && claim.rejects_root(*root)
                                })
                            })
                    })
                    .map(|root| (orphan.id.clone(), root.id.clone()))
            })
            .collect();
        for id in marked.values_mut().chain(roots.values_mut()) {
            if let Some(root) = reconciled.get(id) {
                *id = root.clone();
            }
        }
        for workload in self.workloads.values_mut() {
            if let Some(root) = reconciled.get(&workload.agent_id) {
                workload.agent_id = root.clone();
            }
        }
        for id in nearest.values_mut().flatten() {
            if let Some(root) = reconciled.get(id) {
                *id = root.clone();
            }
        }
        for a in self.agents.values_mut() {
            let root = valid_agents.get(&a.id).copied();
            let unknown = provisional.contains(&a.id);
            let state = if root.is_some() && a.state != AgentState::Ended {
                a.state
            } else if root.is_some() || unknown {
                AgentState::Unknown
            } else {
                AgentState::Ended
            };
            a.root = root;
            if state == AgentState::Ended {
                if a.state != AgentState::Ended {
                    a.ended_at_ms = Some(wall_ms);
                }
            } else {
                a.ended_at_ms = None;
            }
            a.state = state;
        }
        // A provisional assignment is not durable evidence after its context resolves.
        for cached in self.cache.values_mut() {
            let a = &mut cached.attribution;
            let was_provisional = a
                .agent_id
                .as_ref()
                .is_some_and(|id| previously_provisional.contains(id));
            if was_provisional
                && a.agent_id
                    .as_ref()
                    .is_none_or(|id| !provisional.contains(id))
            {
                a.agent_id = None;
                a.workload_id = None;
                a.role = ProcessRole::Unattributed;
            } else if !was_provisional {
                if let Some(root) = a.agent_id.as_ref().and_then(|id| reconciled.get(id)) {
                    a.agent_id = Some(root.clone());
                }
            }
        }
        self.claims = claims
            .into_iter()
            .filter(|(_, claim)| {
                claim.rule != Rule::Owner
                    || claim
                        .agent
                        .as_ref()
                        .and_then(|id| self.agents.get(id))
                        .is_none_or(|a| a.kind == "generic")
            })
            .collect();
        Discovery {
            roots,
            marked,
            nearest,
        }
    }

    pub(super) fn prune_claims(&mut self) {
        let mut retained: Vec<_> = std::mem::take(&mut self.claims)
            .into_iter()
            .filter(|(_, claim)| claim.retained(&self.cache, &self.agents))
            .collect();
        // Newest claimant keeps the least restrictive age bound, erring toward protection.
        retained.sort_by_key(|(_, c)| std::cmp::Reverse(c.claimant.start_time));
        let mut seen = HashSet::new();
        self.claims = retained
            .into_iter()
            .filter(|(_, c)| {
                self.cache.contains_key(&c.claimant)
                    || seen.insert((
                        c.uid,
                        c.rule,
                        c.kind.clone(),
                        c.session.clone(),
                        c.candidates.clone(),
                    ))
            })
            .collect();
    }

    fn record_claim(
        &mut self,
        p: &Process,
        rule: Rule,
        kind: String,
        session: Option<String>,
        pids: Vec<i32>,
    ) {
        let claim = self
            .claims
            .entry((p.identity, rule))
            .or_insert_with(|| Claim {
                claimant: p.identity,
                uid: p.uid,
                rule,
                kind: kind.clone(),
                session: session.clone(),
                candidates: Vec::new(),
                agent: None,
            });
        if claim.session != session || claim.kind != kind {
            claim.candidates.clear();
            claim.agent = None;
        }
        claim.session = session;
        claim.kind = kind;
        let mut old = std::mem::take(&mut claim.candidates);
        claim.candidates = pids
            .into_iter()
            .map(|pid| {
                old.iter()
                    .position(|c| c.pid == pid)
                    .map(|i| old.remove(i))
                    .unwrap_or(Candidate {
                        pid,
                        identity: None,
                        validity: Validity::Unknown,
                        identity_rejected: false,
                    })
            })
            .collect();
        // A detached claimant retains its observed root identity instead of rebinding a PID.
        claim.candidates.extend(old);
    }
}

fn validate(
    c: &mut Candidate,
    claimant: ProcessIdentity,
    uid: u32,
    rule: Rule,
    markers: &[Marker],
    platform: &impl Platform,
    table: &HashMap<i32, &Process>,
) -> Validity {
    if c.identity_rejected {
        return Validity::Invalid;
    }
    let Some(p) = table.get(&c.pid) else {
        let gone = c.identity.map_or_else(
            || platform.pid_is_present(c.pid) == Some(false),
            |id| platform.process_liveness(id) == ProcessLiveness::Gone,
        );
        return if gone {
            Validity::Invalid
        } else if c.validity == Validity::Valid {
            Validity::Valid
        } else {
            Validity::Unknown
        };
    };
    if p.uid != uid
        || p.identity.start_time > claimant.start_time
        || c.identity.is_some_and(|id| id != p.identity)
    {
        c.identity_rejected = true;
        return Validity::Invalid;
    }
    c.identity = Some(p.identity);
    if rule == Rule::Owner {
        return Validity::Valid;
    }
    if binary(p).is_none() {
        return if c.validity == Validity::Valid {
            Validity::Valid
        } else {
            Validity::Unknown
        };
    }
    let matches = match rule {
        Rule::Binary => agent_kind(p).is_some(),
        Rule::Owner => true,
        Rule::Marker(index) => binary(p).is_some_and(|b| {
            markers
                .get(index)
                .is_some_and(|m| m.root_binaries.iter().any(|name| name == b))
        }),
    };
    if matches || (c.validity == Validity::Valid && agent_kind(p).is_some()) {
        Validity::Valid
    } else {
        Validity::Invalid
    }
}

#[allow(clippy::too_many_arguments)]
fn inherited_root(
    p: &Process,
    table: &HashMap<i32, &Process>,
    roots: &HashMap<ProcessIdentity, String>,
    missing: &HashMap<i32, (ProcessIdentity, String)>,
    path_only: &HashSet<ProcessIdentity>,
    memo: &mut HashMap<ProcessIdentity, Option<String>>,
    visiting: &mut HashSet<ProcessIdentity>,
) -> Option<String> {
    if let Some(root) = memo.get(&p.identity) {
        return root.clone();
    }
    if !visiting.insert(p.identity) {
        return None;
    }
    let root = roots
        .get(&p.identity)
        .filter(|_| !path_only.contains(&p.identity))
        .cloned()
        .or_else(|| {
            if let Some(parent) = table.get(&p.ppid) {
                if parent.uid != p.uid || parent.identity.start_time > p.identity.start_time {
                    return None;
                }
                inherited_root(parent, table, roots, missing, path_only, memo, visiting)
            } else {
                missing
                    .get(&p.ppid)
                    .filter(|(id, _)| id.start_time <= p.identity.start_time)
                    .map(|(_, agent)| agent.clone())
            }
        });
    visiting.remove(&p.identity);
    memo.insert(p.identity, root.clone());
    root
}
