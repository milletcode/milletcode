use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::agents::AgentAdapter;

/// Registry of all agent adapters.
pub struct AgentRegistry {
    adapters: RwLock<HashMap<String, Arc<dyn AgentAdapter>>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            adapters: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(&self, adapter: Arc<dyn AgentAdapter>) {
        let adapter_id = adapter.adapter_id().to_string();
        tracing::info!("Registering agent adapter: {}", adapter_id);
        self.adapters.write().insert(adapter_id, adapter);
    }

    pub fn get(&self, adapter_id: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.adapters.read().get(adapter_id).cloned()
    }

    pub fn list(&self) -> Vec<String> {
        self.adapters.read().keys().cloned().collect()
    }

    pub fn has(&self, adapter_id: &str) -> bool {
        self.adapters.read().contains_key(adapter_id)
    }
}
