//! Session-owned cartridge bytes. Publishing is atomic and never writes a file.
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const CHUNK_BYTES: usize = 256 * 1024;
pub const CHUNK_BASE64_MAX: usize = CHUNK_BYTES.div_ceil(3) * 4;
const MEMORY_BYTES: usize = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;
const MAX_CHUNKS: usize = 8192;
const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CartridgeInfo {
    pub name: String,
    pub size: u64,
    pub created_unix_millis: u64,
}

struct Payload {
    // Each chunk keeps its start offset so reads need only visit overlapping chunks.
    chunks: Vec<(usize, Vec<u8>)>,
    size: usize,
    allocated_bytes: usize,
}

struct Pending {
    name: String,
    payload: Payload,
    touched: Instant,
}

struct Cartridge {
    info: CartridgeInfo,
    payload: Payload,
}

pub struct CartridgeStore {
    pending: BTreeMap<String, Pending>,
    committed: BTreeMap<String, Cartridge>,
    used_bytes: usize,
    used_chunks: usize,
    budget: usize,
}

impl Default for CartridgeStore {
    fn default() -> Self {
        Self::with_budget(MEMORY_BYTES)
    }
}

impl CartridgeStore {
    fn with_budget(budget: usize) -> Self {
        Self {
            pending: BTreeMap::new(),
            committed: BTreeMap::new(),
            used_bytes: 0,
            used_chunks: 0,
            budget,
        }
    }

    fn reap(&mut self) {
        let now = Instant::now();
        let used_bytes = &mut self.used_bytes;
        let used_chunks = &mut self.used_chunks;
        self.pending.retain(|_, pending| {
            if now.duration_since(pending.touched) >= UPLOAD_IDLE_TIMEOUT {
                *used_bytes -= pending.payload.allocated_bytes;
                *used_chunks -= pending.payload.chunks.len();
                false
            } else {
                true
            }
        });
    }

    pub fn begin(&mut self, name: String) -> Result<String> {
        self.reap();
        ensure!(valid_name(&name), "Invalid cartridge filename");
        ensure!(
            self.committed.len() + self.pending.len() < MAX_ENTRIES,
            "Cartridge memory entry limit reached; remove a cartridge before exporting another"
        );
        ensure!(
            !self.committed.contains_key(&name)
                && !self.pending.values().any(|pending| pending.name == name),
            "A cartridge with this name already exists"
        );
        let id = uuid::Uuid::new_v4().to_string();
        self.pending.insert(
            id.clone(),
            Pending {
                name,
                payload: Payload {
                    chunks: Vec::new(),
                    size: 0,
                    allocated_bytes: 0,
                },
                touched: Instant::now(),
            },
        );
        Ok(id)
    }

    /// Any refused append aborts the unpublished upload and releases its budget.
    pub fn append(&mut self, id: &str, data: Vec<u8>) -> Result<()> {
        self.reap();
        if !self.pending.contains_key(id) {
            bail!("Cartridge upload does not exist or expired");
        }
        let error = if data.len() > CHUNK_BYTES {
            Some("Cartridge chunk is too large")
        } else if data.capacity() > self.budget.saturating_sub(self.used_bytes) {
            Some("Cartridge memory is full; remove a cartridge before exporting another")
        } else if !data.is_empty() && self.used_chunks >= MAX_CHUNKS {
            Some("Cartridge memory chunk limit reached")
        } else {
            None
        };
        if let Some(error) = error {
            self.abort(id);
            bail!(error);
        }
        let pending = self.pending.get_mut(id).expect("checked upload");
        pending.touched = Instant::now();
        if !data.is_empty() {
            self.used_bytes += data.capacity();
            pending.payload.allocated_bytes += data.capacity();
            self.used_chunks += 1;
            let size = data.len();
            pending.payload.chunks.push((pending.payload.size, data));
            pending.payload.size += size;
        }
        Ok(())
    }

    pub fn commit(&mut self, id: &str) -> Result<CartridgeInfo> {
        self.reap();
        let pending = self
            .pending
            .remove(id)
            .ok_or_else(|| anyhow::anyhow!("Cartridge upload does not exist or expired"))?;
        if pending.payload.size == 0 || self.committed.contains_key(&pending.name) {
            self.used_bytes -= pending.payload.allocated_bytes;
            self.used_chunks -= pending.payload.chunks.len();
            bail!("Cartridge is empty or its filename already exists");
        }
        let info = CartridgeInfo {
            name: pending.name.clone(),
            size: pending.payload.size as u64,
            created_unix_millis: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        };
        self.committed.insert(
            pending.name,
            Cartridge {
                info: info.clone(),
                payload: pending.payload,
            },
        );
        Ok(info)
    }

    pub fn abort(&mut self, id: &str) {
        self.reap();
        if let Some(pending) = self.pending.remove(id) {
            self.used_bytes -= pending.payload.allocated_bytes;
            self.used_chunks -= pending.payload.chunks.len();
        }
    }

    pub fn list(&mut self) -> Vec<CartridgeInfo> {
        self.reap();
        self.committed
            .values()
            .map(|entry| entry.info.clone())
            .collect()
    }

    pub fn info(&mut self, name: &str) -> Option<CartridgeInfo> {
        self.reap();
        self.committed.get(name).map(|entry| entry.info.clone())
    }

    pub fn read(&mut self, name: &str, offset: u64, size: u32) -> Option<Vec<u8>> {
        self.reap();
        let entry = self.committed.get(name)?;
        if offset >= entry.info.size || size == 0 {
            return Some(Vec::new());
        }
        let start = offset as usize; // offset is bounded by the memory budget above.
        let end = start
            .saturating_add((size as usize).min(CHUNK_BYTES))
            .min(entry.payload.size);
        let chunks = &entry.payload.chunks;
        let first = chunks.partition_point(|(at, chunk)| at + chunk.len() <= start);
        let mut bytes = Vec::with_capacity(end - start);
        for (at, chunk) in &chunks[first..] {
            if *at >= end {
                break;
            }
            bytes.extend_from_slice(&chunk[start.saturating_sub(*at)..(end - at).min(chunk.len())]);
        }
        Some(bytes)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.reap();
        if let Some(entry) = self.committed.remove(name) {
            self.used_bytes -= entry.payload.allocated_bytes;
            self.used_chunks -= entry.payload.chunks.len();
            true
        } else {
            false
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.starts_with('.')
        && name.len() <= 255
        && name.len() > 4
        && name.ends_with(".png")
        && !name.contains(['/', '\\', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_is_atomic_and_reads_cross_chunks_in_any_order() {
        let mut store = CartridgeStore::with_budget(100);
        let id = store.begin("track.png".into()).unwrap();
        store.append(&id, b"abc".to_vec()).unwrap();
        store.append(&id, b"defgh".to_vec()).unwrap();
        assert!(store.list().is_empty());
        assert!(store.read("track.png", 0, 5).is_none());
        assert_eq!(store.commit(&id).unwrap().size, 8);
        assert_eq!(store.read("track.png", 5, 99).unwrap(), b"fgh");
        assert_eq!(store.read("track.png", 1, 5).unwrap(), b"bcdef");
        assert!(store.read("track.png", u64::MAX, 99).unwrap().is_empty());
        assert_eq!(store.info("track.png").unwrap().size, 8);
    }

    #[test]
    fn committed_and_pending_share_quota_without_eviction() {
        let mut store = CartridgeStore::with_budget(6);
        let a = store.begin("a.png".into()).unwrap();
        store.append(&a, vec![1; 4]).unwrap();
        store.commit(&a).unwrap();
        let b = store.begin("b.png".into()).unwrap();
        store.append(&b, vec![2; 2]).unwrap();
        assert!(store.append(&b, vec![3]).is_err());
        assert_eq!(store.used_bytes, 4);
        assert!(store.commit(&b).is_err());
        assert_eq!(store.list().len(), 1);
        assert!(store.remove("a.png"));
        assert!(!store.remove("a.png"));
        assert_eq!(store.used_bytes, 0);
        assert_eq!(store.used_chunks, 0);
    }

    #[test]
    fn expired_and_aborted_uploads_release_memory() {
        let mut store = CartridgeStore::with_budget(6);
        let a = store.begin("a.png".into()).unwrap();
        store.append(&a, vec![1; 6]).unwrap();
        store.pending.get_mut(&a).unwrap().touched = Instant::now() - UPLOAD_IDLE_TIMEOUT;
        assert!(store.commit(&a).is_err());
        assert_eq!(store.used_bytes, 0);
        let b = store.begin("b.png".into()).unwrap();
        store.append(&b, vec![1; 6]).unwrap();
        store.abort(&b);
        store.abort(&b);
        assert_eq!(store.used_bytes, 0);
        assert_eq!(store.used_chunks, 0);
    }

    #[test]
    fn name_collisions_and_invalid_names_are_rejected() {
        let mut store = CartridgeStore::default();
        for name in [
            "../bad.png",
            ".hidden.png",
            "a/b.png",
            "a\\b.png",
            "x\0.png",
            "x.jpg",
            ".png",
        ] {
            assert!(store.begin(name.into()).is_err(), "{name:?}");
        }
        assert!(store.begin(format!("{}.png", "a".repeat(252))).is_err());
        let id = store.begin("a.png".into()).unwrap();
        assert!(store.begin("a.png".into()).is_err());
        store.append(&id, vec![1]).unwrap();
        store.commit(&id).unwrap();
        assert!(store.begin("a.png".into()).is_err());
        assert!(store.commit(&id).is_err());
        let empty = store.begin("empty.png".into()).unwrap();
        assert!(store.commit(&empty).is_err());
    }

    #[test]
    fn chunk_bounds_limit_uploads_and_reads() {
        let mut store = CartridgeStore::default();
        let id = store.begin("a.png".into()).unwrap();
        store.append(&id, vec![1; CHUNK_BYTES]).unwrap();
        store.append(&id, vec![2; 20]).unwrap();
        store.commit(&id).unwrap();
        assert_eq!(store.read("a.png", 0, u32::MAX).unwrap().len(), CHUNK_BYTES);
        let bad = store.begin("bad.png".into()).unwrap();
        store.append(&bad, vec![3]).unwrap();
        assert!(store.append(&bad, vec![4; CHUNK_BYTES + 1]).is_err());
        assert!(store.commit(&bad).is_err());
        assert_eq!(store.used_bytes, CHUNK_BYTES + 20);
    }

    #[test]
    fn quota_accounts_for_owned_allocation_capacity() {
        let mut store = CartridgeStore::with_budget(100);
        let id = store.begin("reserved.png".into()).unwrap();
        let mut chunk = Vec::with_capacity(101);
        chunk.push(1);
        assert!(store.append(&id, chunk).is_err());
        assert_eq!(store.used_bytes, 0);
        assert!(store.commit(&id).is_err());
    }

    #[test]
    fn chunk_metadata_is_bounded_even_for_tiny_appends() {
        let mut store = CartridgeStore::default();
        let id = store.begin("tiny.png".into()).unwrap();
        for _ in 0..MAX_CHUNKS {
            store.append(&id, vec![1]).unwrap();
        }
        assert!(store.append(&id, vec![1]).is_err());
        assert_eq!(store.used_chunks, 0);
        assert_eq!(store.used_bytes, 0);
    }

    #[test]
    fn reads_match_original_bytes_across_uneven_chunk_boundaries() {
        let mut store = CartridgeStore::default();
        let id = store.begin("uneven.png".into()).unwrap();
        let expected: Vec<_> = (0..1000).map(|i| (i % 251) as u8).collect();
        for chunk in expected.chunks(37) {
            store.append(&id, chunk.to_vec()).unwrap();
        }
        store.commit(&id).unwrap();
        for offset in (0..1100).step_by(19).rev() {
            for size in [0, 1, 13, 37, 51, 499, 1000] {
                let start = offset.min(expected.len());
                let end = (start + size).min(expected.len());
                assert_eq!(
                    store
                        .read("uneven.png", offset as u64, size as u32)
                        .unwrap(),
                    expected[start..end],
                );
            }
        }
    }

    #[test]
    fn entry_limit_bounds_empty_pending_uploads() {
        let mut store = CartridgeStore::default();
        for index in 0..MAX_ENTRIES {
            store.begin(format!("{index}.png")).unwrap();
        }
        assert!(store.begin("extra.png".into()).is_err());
    }
}
