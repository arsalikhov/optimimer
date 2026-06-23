use crate::models::Workflow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Dead-simple JSON-file backed store. Good enough for a single-node clone;
/// swap for sqlx/Postgres when persistence guarantees matter.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<HashMap<String, Workflow>>>,
    path: PathBuf,
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, Workflow>>(&s).ok())
            .unwrap_or_default();
        Store {
            inner: Arc::new(Mutex::new(map)),
            path,
        }
    }

    fn persist(&self, map: &HashMap<String, Workflow>) {
        if let Ok(json) = serde_json::to_string_pretty(map) {
            let _ = std::fs::write(&self.path, json);
        }
    }

    pub fn list(&self) -> Vec<Workflow> {
        let mut v: Vec<Workflow> = self.inner.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        v
    }

    pub fn get(&self, id: &str) -> Option<Workflow> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    pub fn upsert(&self, wf: Workflow) -> Workflow {
        let mut map = self.inner.lock().unwrap();
        map.insert(wf.id.clone(), wf.clone());
        self.persist(&map);
        wf
    }

    pub fn delete(&self, id: &str) -> bool {
        let mut map = self.inner.lock().unwrap();
        let existed = map.remove(id).is_some();
        if existed {
            self.persist(&map);
        }
        existed
    }
}
