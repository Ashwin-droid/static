//! Bounded access to cartridges held in daemon memory. This module is only a
//! client: every listing, metadata lookup and byte range is requested over D-Bus.
use std::path::{Component, Path};

use super::{Ctx, FsError, FsResult};
use crate::cartridges::{CHUNK_BYTES, CartridgeInfo};

fn valid_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    !name.starts_with('.')
        && name.ends_with(".png")
        && !name.contains('\0')
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

pub(crate) fn names(ctx: &Ctx) -> FsResult<Vec<String>> {
    #[cfg(test)]
    if let Some(backend) = &ctx.test_cartridges {
        return Ok(backend.list().into_iter().map(|info| info.name).collect());
    }
    let mut names: Vec<_> = ctx
        .with_daemon(|daemon| daemon.try_list_memory_cartridges())?
        .into_iter()
        .map(|info| info.name)
        .filter(|name| valid_name(name))
        .collect();
    names.sort();
    Ok(names)
}

pub(crate) fn metadata(ctx: &Ctx, name: &str) -> FsResult<CartridgeInfo> {
    if !valid_name(name) {
        return Err(FsError::NotFound);
    }
    #[cfg(test)]
    if let Some(backend) = &ctx.test_cartridges {
        return backend.info(name).ok_or(FsError::NotFound);
    }
    ctx.with_daemon(|daemon| daemon.try_memory_cartridge_info(name))?
        .ok_or(FsError::NotFound)
}

pub(crate) fn read_range(ctx: &Ctx, name: &str, offset: u64, size: u32) -> FsResult<Vec<u8>> {
    if !valid_name(name) {
        return Err(FsError::NotFound);
    }
    let size = size.min(CHUNK_BYTES as u32);
    #[cfg(test)]
    if let Some(backend) = &ctx.test_cartridges {
        return backend.read(name, offset, size).ok_or(FsError::NotFound);
    }
    ctx.with_daemon(|daemon| daemon.try_read_memory_cartridge(name, offset, size))?
        .ok_or(FsError::NotFound)
}

/// Deletion frees the daemon-owned bytes. Never retry: a lost reply must not
/// turn a successful deletion into a second request against a changed store.
pub(crate) fn remove(ctx: &Ctx, name: &str) -> FsResult<()> {
    if !valid_name(name) {
        return Err(FsError::NotFound);
    }
    #[cfg(test)]
    if let Some(backend) = &ctx.test_cartridges {
        return backend.remove(name).then_some(()).ok_or(FsError::NotFound);
    }
    ctx.with_daemon_once(|daemon| daemon.try_remove_memory_cartridge(name))?
        .then_some(())
        .ok_or(FsError::NotFound)
}

/// An isolated fake daemon backend for tests. It is absent from production;
/// staticfs never owns or caches cartridge payloads in a real mount.
#[cfg(test)]
pub(crate) mod test_backend {
    use super::CartridgeInfo;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub(crate) struct TestCartridges(Arc<Mutex<BTreeMap<String, (CartridgeInfo, Vec<u8>)>>>);

    impl TestCartridges {
        pub(crate) fn insert(&self, name: &str, bytes: &[u8]) {
            self.0.lock().unwrap().insert(
                name.into(),
                (
                    CartridgeInfo {
                        name: name.into(),
                        size: bytes.len() as u64,
                        created_unix_millis: 1234567,
                    },
                    bytes.to_vec(),
                ),
            );
        }
        pub(crate) fn list(&self) -> Vec<CartridgeInfo> {
            self.0
                .lock()
                .unwrap()
                .values()
                .map(|(info, _)| info.clone())
                .collect()
        }
        pub(crate) fn info(&self, name: &str) -> Option<CartridgeInfo> {
            self.0
                .lock()
                .unwrap()
                .get(name)
                .map(|(info, _)| info.clone())
        }
        pub(crate) fn read(&self, name: &str, offset: u64, size: u32) -> Option<Vec<u8>> {
            let guard = self.0.lock().unwrap();
            let (_, bytes) = guard.get(name)?;
            let start = offset.min(bytes.len() as u64) as usize;
            let end = start.saturating_add(size as usize).min(bytes.len());
            Some(bytes[start..end].to_vec())
        }
        pub(crate) fn remove(&self, name: &str) -> bool {
            self.0.lock().unwrap().remove(name).is_some()
        }
    }
}

#[cfg(test)]
#[path = "tests/cartridge_files.rs"]
mod tests;
