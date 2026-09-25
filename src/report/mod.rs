use crate::daemon::{
    Snapshot,
    files::{Mode, Paths},
};
use crate::platform::ProcessIdentity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Freezes {
    pub count: u64,
    pub total_ms: u64,
    pub longest_ms: u64,
    pub peak_memory_bytes: u64,
    pub incomplete_memory_samples: u64,
    pub pressure_after_30s: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Totals {
    pub throttled_workload_ms: u64,
    pub observed_ms: u64,
    pub elevated_ms: u64,
    pub critical_ms: u64,
    pub freezes_by_agent_kind: BTreeMap<String, Freezes>,
    pub holds: u64,
    pub hold_wait_seconds: BTreeMap<u16, u64>,
    pub timed_out_holds: u64,
    pub cancelled_holds: u64,
    pub reclaimed_processes: u64,
    pub reclaimed_memory_bytes: u64,
    pub reclaimed_memory_unknown: u64,
    pub services_left_running: u64,
    pub kills_blocked: u64,
    pub forced_resumes: u64,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Day {
    pub enforce: Totals,
    pub observe: Totals,
}
impl Day {
    fn mode(&mut self, observe: bool) -> &mut Totals {
        if observe {
            &mut self.observe
        } else {
            &mut self.enforce
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Store {
    pub schema_version: u32,
    pub updated_at_ms: Option<u64>,
    pub days: BTreeMap<String, Day>,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            schema_version: 1,
            updated_at_ms: None,
            days: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Summary {
    pub day: String,
    pub enforce: Counts,
    pub observe: Counts,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Counts {
    pub freezes: u64,
    pub holds: u64,
    pub median_wait_seconds: Option<u16>,
    pub reclaimed_memory_bytes: u64,
    pub reclaimed_processes: u64,
    pub services_left_running: u64,
    pub forced_resumes: u64,
    pub kills_blocked: u64,
}
impl From<&Totals> for Counts {
    fn from(t: &Totals) -> Self {
        Self {
            freezes: t.freezes_by_agent_kind.values().map(|f| f.count).sum(),
            holds: t.holds,
            median_wait_seconds: t.median_wait(),
            reclaimed_memory_bytes: t.reclaimed_memory_bytes,
            reclaimed_processes: t.reclaimed_processes,
            services_left_running: t.services_left_running,
            forced_resumes: t.forced_resumes,
            kills_blocked: t.kills_blocked,
        }
    }
}
impl Totals {
    pub fn median_wait(&self) -> Option<u16> {
        let count: u64 = self.hold_wait_seconds.values().sum();
        if count == 0 {
            return None;
        }
        let mut seen = 0;
        let mut lower = None;
        self.hold_wait_seconds.iter().find_map(|(&seconds, &n)| {
            seen += n;
            if seen > (count - 1) / 2 && lower.is_none() {
                lower = Some(seconds);
            }
            (seen > count / 2)
                .then(|| ((u32::from(lower.unwrap()) + u32::from(seconds)) / 2) as u16)
        })
    }
    fn add(&mut self, other: &Self) {
        macro_rules! sum { ($($f:ident),*) => { $(self.$f += other.$f;)* }; }
        sum!(
            throttled_workload_ms,
            observed_ms,
            elevated_ms,
            critical_ms,
            holds,
            timed_out_holds,
            cancelled_holds,
            reclaimed_processes,
            reclaimed_memory_bytes,
            reclaimed_memory_unknown,
            services_left_running,
            kills_blocked,
            forced_resumes
        );
        for (&bucket, &count) in &other.hold_wait_seconds {
            *self.hold_wait_seconds.entry(bucket).or_default() += count;
        }
        for (kind, b) in &other.freezes_by_agent_kind {
            let a = self.freezes_by_agent_kind.entry(kind.clone()).or_default();
            a.count += b.count;
            a.total_ms += b.total_ms;
            a.longest_ms = a.longest_ms.max(b.longest_ms);
            a.peak_memory_bytes = a.peak_memory_bytes.max(b.peak_memory_bytes);
            a.incomplete_memory_samples += b.incomplete_memory_samples;
            for (pair, count) in &b.pressure_after_30s {
                *a.pressure_after_30s.entry(pair.clone()).or_default() += count;
            }
        }
    }
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Report {
    pub schema_version: u32,
    pub updated_at_ms: Option<u64>,
    pub hold_waits: BTreeMap<String, WaitSummary>,
    pub since_days: u16,
    pub from_day: String,
    pub through_day: String,
    pub days: BTreeMap<String, Day>,
    pub totals: Day,
}
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WaitSummary {
    pub completed: u64,
    pub median_seconds: Option<u16>,
    pub worst_seconds: Option<u16>,
}
impl Default for Report {
    fn default() -> Self {
        Self {
            schema_version: 1,
            updated_at_ms: None,
            hold_waits: BTreeMap::new(),
            since_days: 7,
            from_day: String::new(),
            through_day: String::new(),
            days: BTreeMap::new(),
            totals: Day::default(),
        }
    }
}
impl Store {
    pub fn report(&self, since: u16, now: u64) -> io::Result<Report> {
        if ![1, 7, 30, 90].contains(&since) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "since must be 1d, 7d, 30d or 90d",
            ));
        }
        let mut report = Report {
            updated_at_ms: self.updated_at_ms,
            since_days: since,
            from_day: day_offset(now, 1 - i32::from(since)),
            through_day: day_offset(now, 0),
            ..Report::default()
        };
        for (day, totals) in self
            .days
            .range(report.from_day.clone()..=report.through_day.clone())
        {
            report.totals.enforce.add(&totals.enforce);
            report.totals.observe.add(&totals.observe);
            report.days.insert(day.clone(), totals.clone());
        }
        for (mode, t) in [
            ("enforce", &report.totals.enforce),
            ("observe", &report.totals.observe),
        ] {
            report.hold_waits.insert(
                mode.into(),
                WaitSummary {
                    completed: t.hold_wait_seconds.values().sum(),
                    median_seconds: t.median_wait(),
                    worst_seconds: t.hold_wait_seconds.keys().next_back().copied(),
                },
            );
        }
        Ok(report)
    }
    fn summary(&self, now: u64) -> Summary {
        let day = day_offset(now, 0);
        let totals = self.days.get(&day).cloned().unwrap_or_default();
        Summary {
            day,
            enforce: Counts::from(&totals.enforce),
            observe: Counts::from(&totals.observe),
        }
    }
    fn totals(&mut self, at: u64, observe: bool) -> &mut Totals {
        self.days
            .entry(day_offset(at, 0))
            .or_default()
            .mode(observe)
    }
    fn prune(&mut self, now: u64) {
        let oldest = day_offset(now, -89);
        self.days.retain(|day, _| day >= &oldest);
    }
}

fn local_time(at: u64) -> libc::tm {
    let seconds = (at / 1000) as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed() };
    unsafe {
        #[cfg(target_os = "linux")]
        {
            unsafe extern "C" {
                fn tzset();
            }
            tzset();
        }
        libc::localtime_r(&seconds, &mut tm);
    }
    tm
}
pub(crate) fn day_offset(at: u64, offset: i32) -> String {
    let mut tm = local_time(at);
    tm.tm_mday += offset;
    tm.tm_hour = 12;
    tm.tm_isdst = -1;
    unsafe {
        libc::mktime(&mut tm);
    }
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}
fn next_midnight(at: u64) -> u64 {
    let mut tm = local_time(at);
    tm.tm_mday += 1;
    tm.tm_hour = 0;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    unsafe { libc::mktime(&mut tm) as u64 * 1000 }
}

pub fn read_or_empty(paths: &Paths) -> Store {
    read(paths).unwrap_or_else(|e| {
        eprintln!("stats unavailable, starting fresh: {e}");
        Store::default()
    })
}
pub fn read(paths: &Paths) -> io::Result<Store> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(paths.base.join("state/stats.json"))
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("stats must be a regular file"));
    }
    let mut data = Vec::new();
    file.take(32 * 1024 * 1024).read_to_end(&mut data)?;
    let store: Store = serde_json::from_slice(&data)?;
    if store.schema_version != 1 {
        return Err(io::Error::other("unsupported stats schema"));
    }
    Ok(store)
}
fn write(paths: &Paths, store: &Store) -> io::Result<()> {
    let temporary = paths.base.join("state/stats.json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&temporary)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("stats must be a regular file"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer(&mut file, store)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, paths.base.join("state/stats.json"))?;
    File::open(paths.base.join("state"))?.sync_all()
}

pub(crate) enum Message {
    Decision {
        at: u64,
        event: String,
        details: Value,
    },
    Sample {
        at: u64,
        observe: bool,
        level: Option<String>,
        frozen: Vec<(String, u64, bool)>,
        throttled: Vec<String>,
    },
}
#[derive(Default)]
struct Pending {
    latest: Option<Store>,
    decision: bool,
    stopping: bool,
}
type Mailbox = Arc<(Mutex<Pending>, Condvar)>;
#[derive(Clone)]
pub(crate) struct Recorder {
    engine: Arc<Mutex<Engine>>,
    mailbox: Mailbox,
}
impl Recorder {
    pub(crate) fn record(&self, message: Message) -> Summary {
        let decision = matches!(&message, Message::Decision { .. });
        let mut engine = self.engine.lock().unwrap();
        let at = engine.apply(message);
        engine.store.prune(at);
        let summary = engine.store.summary(at);
        let mut pending = self.mailbox.0.lock().unwrap();
        pending.latest = Some(engine.store.clone());
        pending.decision |= decision;
        self.mailbox.1.notify_one();
        summary
    }
}
pub struct Worker {
    pub(crate) recorder: Recorder,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Worker {
    pub fn start(paths: Paths) -> io::Result<Self> {
        Self::with_writer(read_or_empty(&paths), move |store| write(&paths, store))
    }
    fn with_writer(
        store: Store,
        persist: impl FnMut(&Store) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let mailbox = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let recorder = Recorder {
            engine: Arc::new(Mutex::new(Engine::new(store))),
            mailbox: Arc::clone(&mailbox),
        };
        let thread = std::thread::Builder::new()
            .name("stats".into())
            .spawn(move || run_worker(mailbox, persist))?;
        Ok(Self {
            recorder,
            thread: Some(thread),
        })
    }
    pub fn sample(&self, s: &Snapshot) -> Summary {
        let frozen = s
            .frozen
            .iter()
            .map(|f| {
                let memory = s
                    .attribution
                    .workloads
                    .iter()
                    .find(|w| w.id == f.workload_id);
                (
                    f.workload_id.clone(),
                    memory.map_or(0, |w| w.memory.bytes),
                    memory.is_some_and(|w| w.memory.complete) && !s.status.sample_discarded,
                )
            })
            .collect();
        self.recorder.record(Message::Sample {
            at: s.status.sampled_at_ms,
            observe: matches!(s.status.mode, Mode::Observe),
            level: (!s.status.sample_discarded && s.pressure.is_some())
                .then(|| level(s.status.pressure_level)),
            frozen,
            throttled: s
                .guardian
                .iter()
                .filter_map(|n| n.throttle.as_ref())
                .flat_map(|t| t.workloads.iter().map(|w| w.workload_id.clone()))
                .collect(),
        })
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.recorder.mailbox.0.lock().unwrap().stopping = true;
        self.recorder.mailbox.1.notify_one();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                eprintln!("stats worker failed during shutdown");
            }
        }
    }
}
fn run_worker(mailbox: Mailbox, mut persist: impl FnMut(&Store) -> io::Result<()>) {
    let mut last_write = Instant::now();
    let interval = Duration::from_secs(30);
    let mut retry = None;
    loop {
        let mut pending = mailbox.0.lock().unwrap();
        loop {
            let dirty = pending.latest.is_some() || retry.is_some();
            if pending.stopping || (dirty && (pending.decision || last_write.elapsed() >= interval))
            {
                break;
            }
            let wait = if dirty {
                interval.saturating_sub(last_write.elapsed())
            } else {
                interval
            };
            pending = mailbox.1.wait_timeout(pending, wait).unwrap().0;
        }
        let stopping = pending.stopping;
        let latest = pending.latest.take().or_else(|| retry.take());
        pending.decision = false;
        drop(pending);
        // No producer lock is held during serialization or disk I/O.
        if let Some(store) = latest {
            match persist(&store) {
                Ok(()) => retry = None,
                Err(error) => {
                    eprintln!("stats write failed: {error}");
                    retry = Some(store);
                }
            }
            last_write = Instant::now();
        }
        if stopping {
            break;
        }
    }
}
fn level(value: crate::guardian::Level) -> String {
    format!("{value:?}").to_lowercase()
}
struct Frozen {
    kind: String,
    observe: bool,
    last: u64,
    elapsed: u64,
}
struct Comparison {
    at: u64,
    day: String,
    kind: String,
    observe: bool,
    before: String,
}
struct Engine {
    throttled: HashMap<String, (u64, bool)>,
    store: Store,
    previous: Option<(u64, bool, Option<String>)>,
    frozen: HashMap<String, Frozen>,
    comparisons: Vec<Comparison>,
    reclaim: HashMap<ProcessIdentity, Option<u64>>,
}
impl Engine {
    fn new(store: Store) -> Self {
        Self {
            store,
            throttled: HashMap::new(),
            previous: None,
            frozen: HashMap::new(),
            comparisons: Vec::new(),
            reclaim: HashMap::new(),
        }
    }
    fn apply(&mut self, message: Message) -> u64 {
        let at = match message {
            Message::Decision { at, event, details } => {
                self.decision(at, &event, &details);
                at
            }
            Message::Sample {
                at,
                observe,
                level,
                frozen,
                throttled,
            } => {
                if let Some((last, mode, Some(previous))) = self.previous.take() {
                    if at >= last && at - last <= 5000 && level.is_some() && mode == observe {
                        let mut cursor = last;
                        while cursor < at {
                            let end = next_midnight(cursor).min(at);
                            let t = self.store.totals(cursor, observe);
                            t.observed_ms += end - cursor;
                            match previous.as_str() {
                                "elevated" => t.elevated_ms += end - cursor,
                                "critical" => t.critical_ms += end - cursor,
                                _ => {}
                            }
                            cursor = end;
                        }
                    }
                }
                self.previous = Some((at, observe, level.clone()));
                self.advance_frozen(at);
                self.advance_throttled(at);
                self.throttled
                    .retain(|id, (_, mode)| *mode == observe && throttled.contains(id));
                for id in throttled {
                    self.throttled.entry(id).or_insert((at, observe));
                }
                let mut memories: HashMap<(bool, String), (u64, bool)> = HashMap::new();
                for (id, bytes, complete) in frozen {
                    if let Some(f) = self.frozen.get(&id) {
                        let entry = memories
                            .entry((f.observe, f.kind.clone()))
                            .or_insert((0, true));
                        entry.0 += bytes;
                        entry.1 &= complete;
                    }
                }
                for ((mode, kind), (bytes, complete)) in memories {
                    let f = self
                        .store
                        .totals(at, mode)
                        .freezes_by_agent_kind
                        .entry(kind)
                        .or_default();
                    f.peak_memory_bytes = f.peak_memory_bytes.max(bytes);
                    f.incomplete_memory_samples += u64::from(!complete);
                }
                if let Some(after) = level {
                    self.comparisons.retain(|c| {
                        if at.saturating_sub(c.at) < 30_000 {
                            return true;
                        }
                        if let Some(day) = self.store.days.get_mut(&c.day) {
                            let f = day
                                .mode(c.observe)
                                .freezes_by_agent_kind
                                .entry(c.kind.clone())
                                .or_default();
                            if let Some(n) = f
                                .pressure_after_30s
                                .get_mut(&format!("{}->unknown", c.before))
                            {
                                *n = n.saturating_sub(1);
                            }
                            f.pressure_after_30s.retain(|_, n| *n > 0);
                            *f.pressure_after_30s
                                .entry(format!("{}->{after}", c.before))
                                .or_default() += 1;
                        }
                        false
                    });
                }
                at
            }
        };
        self.store.updated_at_ms = Some(self.store.updated_at_ms.unwrap_or(0).max(at));
        at
    }
    fn advance_frozen(&mut self, at: u64) {
        for f in self.frozen.values_mut() {
            if at < f.last {
                continue;
            }
            if at >= f.last && at - f.last <= 5000 {
                let mut cursor = f.last;
                while cursor < at {
                    let end = next_midnight(cursor).min(at);
                    f.elapsed += end - cursor;
                    let totals = self
                        .store
                        .totals(cursor, f.observe)
                        .freezes_by_agent_kind
                        .entry(f.kind.clone())
                        .or_default();
                    totals.total_ms += end - cursor;
                    totals.longest_ms = totals.longest_ms.max(f.elapsed);
                    cursor = end;
                }
            }
            f.last = at;
        }
    }
    fn advance_throttled(&mut self, at: u64) {
        for (last, observe) in self.throttled.values_mut() {
            if at < *last {
                continue;
            }
            if at - *last <= 5000 {
                let mut cursor = *last;
                while cursor < at {
                    let end = next_midnight(cursor).min(at);
                    self.store.totals(cursor, *observe).throttled_workload_ms += end - cursor;
                    cursor = end;
                }
            }
            *last = at;
        }
    }
    fn decision(&mut self, at: u64, event: &str, v: &Value) {
        let observe = v["mode"] == "observe";
        let d = &v["decision"];
        match event {
            "throttle" => {
                if let Some(id) = d["workload_id"].as_str() {
                    self.throttled.entry(id.to_owned()).or_insert((at, observe));
                }
            }
            "unthrottle" => {
                self.advance_throttled(at);
                if let Some(id) = d["workload_id"].as_str() {
                    self.throttled.remove(id);
                }
            }
            "freeze" => {
                let Some(id) = d["workload_id"].as_str() else {
                    return;
                };
                let kind = d["agent_kind"].as_str().unwrap_or("unknown").to_owned();
                let before = v["level"].as_str().unwrap_or("unknown").to_owned();
                let f = self
                    .store
                    .totals(at, observe)
                    .freezes_by_agent_kind
                    .entry(kind.clone())
                    .or_default();
                f.count += 1;
                *f.pressure_after_30s
                    .entry(format!("{before}->unknown"))
                    .or_default() += 1;
                self.frozen.insert(
                    id.into(),
                    Frozen {
                        kind: kind.clone(),
                        observe,
                        last: at,
                        elapsed: 0,
                    },
                );
                self.comparisons.push(Comparison {
                    at,
                    day: day_offset(at, 0),
                    kind,
                    observe,
                    before,
                });
            }
            "resume" => {
                self.advance_frozen(at);
                if let Some(id) = v["workload"]["workload_id"].as_str() {
                    self.frozen.remove(id);
                }
                if v["reason"] == "max_freeze" {
                    self.store.totals(at, observe).forced_resumes += 1;
                }
            }
            "hold" => self.store.totals(at, observe).holds += 1,
            "hold_completed" => {
                let t = self.store.totals(at, observe);
                let seconds = (v["wait_ms"].as_u64().unwrap_or(0) / 1000).min(300) as u16;
                *t.hold_wait_seconds.entry(seconds).or_default() += 1;
                t.timed_out_holds += u64::from(v["reason"] == "max hold");
                t.cancelled_holds += u64::from(v["reason"] == "cancelled");
            }
            "deny" => self.store.totals(at, observe).kills_blocked += 1,
            "service_reported" => {
                self.store.totals(at, observe).services_left_running +=
                    d["count"].as_u64().unwrap_or(0)
            }
            "clean"
                if !observe
                    && d["error"].is_null()
                    && (d["signal"] == "Terminate" || d["signal"] == "Kill")
                    && d["agent"]["state"] == "ended" =>
            {
                if let Ok(id) = serde_json::from_value(d["process"].clone()) {
                    let memory = d["memory_bytes"].as_u64();
                    self.reclaim
                        .entry(id)
                        .and_modify(|last| {
                            if memory.is_some() {
                                *last = memory;
                            }
                        })
                        .or_insert(memory);
                }
            }
            "clean_reclaimed" if !observe => {
                if let Some(processes) = d["processes"].as_array() {
                    for process in processes {
                        if let Ok(id) = serde_json::from_value(process.clone()) {
                            if let Some(memory) = self.reclaim.remove(&id) {
                                let t = self.store.totals(at, false);
                                t.reclaimed_processes += 1;
                                t.reclaimed_memory_bytes += memory.unwrap_or(0);
                                t.reclaimed_memory_unknown += u64::from(memory.is_none());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
