use anyhow::Result;
use ethers::types::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    last_scanned_block: u64,
    alerted_pools: HashSet<Address>,
}

pub struct Storage {
    path: PathBuf,
    state: StateFile,
}

impl Storage {
    pub fn load_or_default(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let state = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            StateFile::default()
        };
        Ok(Self { path, state })
    }

    pub fn last_scanned_block(&self) -> u64 {
        self.state.last_scanned_block
    }

    pub fn set_last_scanned_block(&mut self, block: u64) -> Result<()> {
        self.state.last_scanned_block = block;
        self.persist()
    }

    pub fn already_alerted(&self, pool: Address) -> bool {
        self.state.alerted_pools.contains(&pool)
    }

    pub fn mark_alerted(&mut self, pool: Address) -> Result<()> {
        self.state.alerted_pools.insert(pool);
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&self.state)?;
        std::fs::write(&self.path, raw)?;
        Ok(())
    }
}
