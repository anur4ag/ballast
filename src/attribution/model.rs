use crate::platform::ProcessIdentity;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Owner {
    pub id: String,
    pub name: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Unknown,
    Thinking,
    Idle,
    Ended,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadClass {
    Batch,
    Service,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRole {
    Unattributed,
    AgentRoot,
    AgentInternal,
    Workload,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemorySummary {
    pub bytes: u64,
    /// Whether every live member has a memory sample in this tick.
    pub complete: bool,
    /// None while warming up or after a gap/unknown memory sample.
    pub growth_30s_bytes: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub owner_id: Option<String>,
    pub session_id: Option<String>,
    pub kind: String,
    pub root: Option<ProcessIdentity>,
    pub cwd: Option<String>,
    pub state: AgentState,
    pub ended_at_ms: Option<u64>,
    pub memory: MemorySummary,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workload {
    pub id: String,
    pub agent_id: String,
    pub root: ProcessIdentity,
    pub label: String,
    pub class: WorkloadClass,
    pub first_seen_ms: u64,
    pub detached_pgid: Option<i32>,
    pub memory: MemorySummary,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessAttribution {
    pub identity: ProcessIdentity,
    pub owner_id: Option<String>,
    pub agent_id: Option<String>,
    pub workload_id: Option<String>,
    pub role: ProcessRole,
    pub environment_known: bool,
    pub listening_ports: Option<Vec<u16>>,
    /// Time of the last successful port lookup; failures leave the sample and time intact.
    pub ports_sampled_at_ms: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AttributionSnapshot {
    pub owners: Vec<Owner>,
    pub agents: Vec<Agent>,
    pub workloads: Vec<Workload>,
    pub processes: Vec<ProcessAttribution>,
}
