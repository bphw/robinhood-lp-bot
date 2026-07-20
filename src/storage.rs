use crate::models::Position;
use anyhow::Result;
use ethers::types::Address;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    last_scanned_block: u64,
    alerted_pools: HashSet<Address>,
    #[serde(default)]
    positions: HashMap<u64, Position>,
    #[serde(default)]
    tpsl_alerted: HashSet<u64>,
    #[serde(default)]
    last_spike_alert_block: HashMap<Address, u64>,
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

    pub fn add_position(&mut self, position: Position) -> Result<()> {
        self.state.positions.insert(position.token_id, position);
        self.persist()
    }

    pub fn open_positions(&self) -> Vec<Position> {
        self.state
            .positions
            .values()
            .filter(|p| !p.closed)
            .cloned()
            .collect()
    }

    #[allow(dead_code)]
    pub fn get_position(&self, token_id: u64) -> Option<Position> {
        self.state.positions.get(&token_id).cloned()
    }

    pub fn mark_position_closed(&mut self, token_id: u64) -> Result<()> {
        if let Some(p) = self.state.positions.get_mut(&token_id) {
            p.closed = true;
        }
        self.persist()
    }

    pub fn already_tpsl_alerted(&self, token_id: u64) -> bool {
        self.state.tpsl_alerted.contains(&token_id)
    }

    pub fn mark_tpsl_alerted(&mut self, token_id: u64) -> Result<()> {
        self.state.tpsl_alerted.insert(token_id);
        self.persist()
    }

    /// Returns true if we're still within the cooldown window since the last
    /// spike alert for this pool (i.e. we should NOT alert again yet).
    pub fn spike_alert_on_cooldown(&self, pool: Address, current_block: u64, cooldown_blocks: u64) -> bool {
        match self.state.last_spike_alert_block.get(&pool) {
            Some(&last) => current_block.saturating_sub(last) < cooldown_blocks,
            None => false,
        }
    }

    pub fn mark_spike_alerted(&mut self, pool: Address, current_block: u64) -> Result<()> {
        self.state.last_spike_alert_block.insert(pool, current_block);
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&self.state)?;
        std::fs::write(&self.path, raw)?;
        Ok(())
    }
}
