use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A node in the workflow graph. `data` is a free-form bag whose meaning
/// depends on `node_type` (see engine.rs for how each type is interpreted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub position: Position,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub source: String,
    pub target: String,
    #[serde(rename = "sourceHandle", default, skip_serializing_if = "Option::is_none")]
    pub source_handle: Option<String>,
    #[serde(rename = "targetHandle", default, skip_serializing_if = "Option::is_none")]
    pub target_handle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub updated_at: String,
}

/// Payload for creating/updating a workflow (id assigned by server on create).
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowInput {
    pub name: String,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunRequest {
    /// Optional inline workflow (the unsaved canvas state). If absent, the
    /// stored workflow referenced by the URL id is run instead.
    #[serde(default)]
    pub workflow: Option<Workflow>,
    /// Initial trigger payload available to the graph as `{{input}}`.
    #[serde(default)]
    pub input: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeResult {
    pub node_id: String,
    pub node_type: String,
    pub status: String, // "ok" | "error" | "skipped"
    pub output: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub ms: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunResponse {
    pub status: String,
    pub results: Vec<NodeResult>,
}
