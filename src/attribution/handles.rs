use crate::daemon::Snapshot;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

#[cfg(test)]
#[path = "handles_tests.rs"]
mod tests;

pub struct WorkloadHandles(BTreeMap<String, String>);

impl WorkloadHandles {
    pub fn new<'a>(ids: impl IntoIterator<Item = &'a str>) -> Self {
        let mut hashes: Vec<_> = ids
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|id| (format!("{:x}", Sha256::digest(id.as_bytes())), id))
            .collect();
        hashes.sort_unstable();
        Self(
            hashes
                .iter()
                .enumerate()
                .map(|(index, (hash, id))| {
                    let length = [index.checked_sub(1), Some(index + 1)]
                        .into_iter()
                        .flatten()
                        .filter_map(|neighbor| hashes.get(neighbor))
                        .map(|(other, _)| {
                            hash.bytes()
                                .zip(other.bytes())
                                .take_while(|(a, b)| a == b)
                                .count()
                                + 1
                        })
                        .max()
                        .unwrap_or(0)
                        .clamp(6, hash.len());
                    ((*id).to_owned(), hash[..length].to_owned())
                })
                .collect(),
        )
    }

    pub fn for_snapshot(snapshot: &Snapshot) -> Self {
        Self::new(
            snapshot
                .attribution
                .workloads
                .iter()
                .map(|w| w.id.as_str())
                .chain(snapshot.frozen.iter().map(|w| w.workload_id.as_str()))
                .chain(
                    snapshot
                        .status
                        .cleanup_pending
                        .iter()
                        .filter(|id| !id.starts_with("internal:"))
                        .map(String::as_str),
                ),
        )
    }

    pub fn get<'a>(&'a self, id: &'a str) -> &'a str {
        self.0.get(id).map(String::as_str).unwrap_or(id)
    }

    pub fn resolve(&self, target: &str) -> io::Result<String> {
        if self.0.contains_key(target) {
            return Ok(target.to_owned());
        }
        let candidates: Vec<_> = self
            .0
            .iter()
            .filter(|(id, _)| {
                target.len() >= 6
                    && format!("{:x}", Sha256::digest(id.as_bytes())).starts_with(target)
            })
            .collect();
        match candidates.as_slice() {
            [(id, _)] => Ok((*id).clone()),
            [] => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "workload not found; use ballast ps for handles",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "ambiguous workload handle, candidates: {}",
                    candidates
                        .iter()
                        .map(|(_, handle)| handle.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }
}
