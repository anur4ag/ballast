use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Paths {
    pub base: PathBuf,
}
impl Paths {
    pub fn from_env() -> io::Result<Self> {
        let base = match std::env::var_os("BALLAST_HOME") {
            Some(base) if !base.is_empty() => PathBuf::from(base),
            Some(_) => return Err(io::Error::other("BALLAST_HOME must not be empty")),
            None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
                io::Error::other("set HOME or BALLAST_HOME to locate daemon files")
            })?)
            .join(".ballast"),
        };
        Ok(Self { base })
    }
    pub fn socket(&self) -> PathBuf {
        self.base.join("run/ballastd.sock")
    }
    pub fn prepare(&self) -> io::Result<()> {
        for dir in [
            self.base.clone(),
            self.base.join("run"),
            self.base.join("state"),
            self.base.join("log"),
        ] {
            private_dir(&dir)?;
        }
        Ok(())
    }
}

pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(io::Error::other("daemon directory must not be a symlink"));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

pub(crate) fn private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("daemon file must be a regular file"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Enforce,
    Observe,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub cleanup_grace_seconds: u64,
    pub pressure: crate::guardian::Thresholds,
    pub markers: Vec<crate::attribution::Marker>,
    pub recovery_sweep_markers: Option<Vec<String>>,
    pub shells: Vec<String>,
    pub heavy_commands: Vec<String>,
    pub log_max_bytes: u64,
    pub log_rotations: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Enforce,
            cleanup_grace_seconds: 30,
            pressure: crate::guardian::Thresholds::default(),
            markers: Vec::new(),
            recovery_sweep_markers: None,
            shells: Vec::new(),
            heavy_commands: Vec::new(),
            log_max_bytes: 5 * 1024 * 1024,
            log_rotations: 3,
        }
    }
}
impl Config {
    pub fn load(paths: &Paths) -> io::Result<Self> {
        let text = match fs::read_to_string(paths.base.join("config.toml")) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let config: Self = toml::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("config.toml: {e}")))?;
        if !config.pressure.valid() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pressure thresholds must be finite, positive, ordered, and PSI percentages at most 100",
            ));
        }
        if config.log_max_bytes == 0 || !(1..=10).contains(&config.log_rotations) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "log_max_bytes must be positive and log_rotations must be 1..=10",
            ));
        }
        let mut keys = std::collections::HashSet::new();
        if crate::attribution::Marker::builtins()
            .iter()
            .chain(&config.markers)
            .any(|m| !m.valid() || !keys.insert(m.key.clone()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "markers must have valid, unique keys and agent kinds",
            ));
        }
        if config
            .recovery_sweep_markers
            .as_ref()
            .is_some_and(|selected| selected.iter().any(|key| !keys.contains(key)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recovery_sweep_markers must name built-in or custom marker keys",
            ));
        }
        if config
            .shells
            .iter()
            .any(|shell| shell.is_empty() || shell.contains(['/', '\0']))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shells must contain executable basenames",
            ));
        }
        Ok(config)
    }
}

pub struct RotatingLog {
    path: PathBuf,
    file: File,
    size: u64,
    max_bytes: u64,
    rotations: usize,
}
impl RotatingLog {
    pub fn open(path: PathBuf, config: &Config) -> io::Result<Self> {
        let file = private_file(&path)?;
        Ok(Self {
            size: file.metadata()?.len(),
            file,
            path,
            max_bytes: config.log_max_bytes,
            rotations: config.log_rotations,
        })
    }
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let length = line.len() as u64 + 1;
        if length > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log record exceeds log_max_bytes",
            ));
        }
        if self.size + length > self.max_bytes {
            for index in (1..=self.rotations).rev() {
                let from = if index == 1 {
                    self.path.clone()
                } else {
                    self.archive(index - 1)
                };
                match fs::rename(from, self.archive(index)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            self.file = private_file(&self.path)?;
            self.size = 0;
        }
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.size += length;
        Ok(())
    }
    fn archive(&self, index: usize) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(format!(".{index}"));
        PathBuf::from(path)
    }
    pub fn decision(&mut self, event: &str, details: serde_json::Value) -> io::Result<()> {
        self.write_line(&serde_json::to_string(&serde_json::json!({
            "timestamp_ms": super::unix_ms(), "event": event, "details": details,
        }))?)
    }
}
