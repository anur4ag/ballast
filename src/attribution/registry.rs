use crate::platform::Process;
use serde::Deserialize;
use std::path::Path;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub key: String,
    pub level: MarkerLevel,
    pub name_key: Option<String>,
    pub kind: Option<String>,
    pub root_pid_key: Option<String>,
    #[serde(default)]
    pub root_binaries: Vec<String>,
    #[serde(default = "yes")]
    pub session_id: bool,
}
fn yes() -> bool {
    true
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MarkerLevel {
    Owner,
    Agent,
}

impl Marker {
    pub fn builtins() -> Vec<Self> {
        toml::from_str::<Registry>(include_str!("markers.toml"))
            .expect("built-in markers")
            .markers
    }
    pub fn valid(&self) -> bool {
        !self.key.is_empty()
            && !self.key.contains(['=', '\0'])
            && self.name_key.as_ref().is_none_or(|s| !s.is_empty())
            && self.root_pid_key.as_ref().is_none_or(|s| !s.is_empty())
            && self.kind.as_ref().is_none_or(|s| !s.is_empty())
            && self
                .root_binaries
                .iter()
                .all(|s| !s.is_empty() && !s.contains('/'))
            && (self.level == MarkerLevel::Owner || self.kind.is_some())
    }
}
#[derive(Deserialize)]
struct Registry {
    markers: Vec<Marker>,
    binaries: Vec<BinaryRule>,
    shells: Vec<String>,
}
#[derive(Deserialize)]
struct BinaryRule {
    name: String,
    kind: String,
    exe_basename: Option<String>,
    exe_path: Option<String>,
    argv0_basename: Option<String>,
}
fn binary_rules() -> &'static [BinaryRule] {
    static RULES: std::sync::OnceLock<Vec<BinaryRule>> = std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        toml::from_str::<Registry>(include_str!("markers.toml"))
            .expect("built-in binary rules")
            .binaries
    })
}
fn matching_rule(p: &Process) -> Option<(&BinaryRule, bool)> {
    let exe = p.exe.as_deref()?;
    let basename = Path::new(exe).file_name()?.to_str()?;
    let argv0 = p
        .argv
        .as_ref()
        .and_then(|a| a.first())
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str());
    binary_rules().iter().find_map(|rule| {
        let path_matches = rule.exe_basename.as_deref() == Some(basename)
            || rule.exe_path.as_ref().is_some_and(|pattern| {
                let mut rest = exe;
                for (i, part) in pattern.split('*').enumerate() {
                    let Some(at) = rest.find(part) else {
                        return false;
                    };
                    if i == 0 && at != 0 {
                        return false;
                    }
                    rest = &rest[at + part.len()..];
                }
                pattern.ends_with('*') || rest.is_empty()
            });
        if !path_matches {
            return None;
        }
        match (rule.argv0_basename.as_deref(), argv0) {
            (Some(_), None) => Some((rule, false)),
            (Some(expected), Some(actual)) if expected != actual => None,
            _ => Some((rule, true)),
        }
    })
}

pub(super) fn binary_candidate_kind(p: &Process) -> Option<&str> {
    matching_rule(p).map(|(rule, _)| rule.kind.as_str())
}

pub(super) fn binary(p: &Process) -> Option<&str> {
    if let Some((rule, known)) = matching_rule(p) {
        return known.then_some(rule.name.as_str());
    }
    Path::new(p.exe.as_deref()?).file_name()?.to_str()
}

pub(super) fn agent_kind(p: &Process) -> Option<&str> {
    binary(p).and_then(|name| {
        binary_rules()
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.kind.as_str())
    })
}

pub(super) fn builtin_shells() -> Vec<String> {
    toml::from_str::<Registry>(include_str!("markers.toml"))
        .expect("built-in shells")
        .shells
}
