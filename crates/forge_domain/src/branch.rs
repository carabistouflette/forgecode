use std::path::PathBuf;

use chrono::{DateTime, Utc};
use derive_setters::Setters;
use serde::{Deserialize, Serialize};

use crate::AgentId;

/// Unique identifier for a managed branch
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BranchId(String);

impl BranchId {
    /// Create a new branch ID from a string
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Get the branch ID as a string
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BranchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Represents the status of a managed branch
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BranchStatus {
    /// Branch is active and being worked on
    #[default]
    Active,
    /// Branch has been merged into main
    Merged,
    /// Branch was abandoned without merging
    Abandoned,
}

/// Represents a managed branch with its work directory and metadata
#[derive(Debug, Clone, Serialize, Deserialize, Setters)]
#[setters(into, strip_option)]
pub struct Branch {
    /// Unique identifier for this branch
    pub id: BranchId,
    /// Git branch name
    pub name: String,
    /// Human-readable task description
    pub task: String,
    /// Path to the branch's work directory
    pub work_dir: PathBuf,
    /// Agent assigned to this branch (if any)
    pub agent_id: Option<AgentId>,
    /// When the branch was created
    pub created_at: DateTime<Utc>,
    /// Current status of the branch
    pub status: BranchStatus,
}

impl Branch {
    /// Create a new branch with the given name and task
    pub fn new(name: impl Into<String>, task: impl Into<String>, work_dir: PathBuf) -> Self {
        let name_str = name.into();
        let id = BranchId::new(name_str.clone());
        Self {
            id,
            name: name_str,
            task: task.into(),
            work_dir,
            agent_id: None,
            created_at: Utc::now(),
            status: BranchStatus::Active,
        }
    }

    /// Set the agent assigned to this branch
    pub fn agent(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    /// Check if this branch has uncommitted changes
    pub fn is_dirty(&self) -> bool {
        // This will be computed at runtime by checking git status
        false
    }
}

/// Lightweight information about a branch for listing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfo {
    /// Unique identifier for this branch
    pub id: BranchId,
    /// Git branch name
    pub name: String,
    /// Human-readable task description
    pub task: String,
    /// Path to the branch's work directory
    pub work_dir: PathBuf,
    /// Agent assigned to this branch (if any)
    pub agent_id: Option<AgentId>,
    /// Current status of the branch
    pub status: BranchStatus,
    /// Whether the branch has uncommitted changes
    pub has_changes: bool,
    /// When the branch was created
    pub created_at: DateTime<Utc>,
}

impl From<&Branch> for BranchInfo {
    fn from(branch: &Branch) -> Self {
        Self {
            id: branch.id.clone(),
            name: branch.name.clone(),
            task: branch.task.clone(),
            work_dir: branch.work_dir.clone(),
            agent_id: branch.agent_id.clone(),
            status: branch.status,
            has_changes: branch.is_dirty(),
            created_at: branch.created_at,
        }
    }
}

/// Result of a branch merge operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeResult {
    /// The branch that was merged
    pub branch: BranchId,
    /// Whether the merge was successful
    pub success: bool,
    /// Whether there were conflicts
    pub has_conflicts: bool,
    /// Commit hash of the merge (if successful)
    pub commit_hash: Option<String>,
    /// Human-readable message describing the result
    pub message: String,
}

/// Branch registry for storing branch metadata
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchRegistry {
    /// All managed branches
    pub branches: Vec<Branch>,
    /// Currently active branch ID (if any)
    pub active_branch: Option<BranchId>,
}

impl BranchRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a branch to the registry
    pub fn add_branch(&mut self, branch: Branch) {
        self.branches.push(branch);
    }

    /// Get a branch by ID
    pub fn get(&self, id: &BranchId) -> Option<&Branch> {
        self.branches.iter().find(|b| &b.id == id)
    }

    /// Get a branch by ID (mutable)
    pub fn get_mut(&mut self, id: &BranchId) -> Option<&mut Branch> {
        self.branches.iter_mut().find(|b| &b.id == id)
    }

    /// Remove a branch from the registry
    pub fn remove(&mut self, id: &BranchId) -> Option<Branch> {
        if self.active_branch.as_ref() == Some(id) {
            self.active_branch = None;
        }
        let pos = self.branches.iter().position(|b| &b.id == id);
        pos.map(|i| self.branches.remove(i))
    }

    /// Set the active branch
    pub fn set_active(&mut self, id: &BranchId) -> Option<&Branch> {
        if !self.branches.iter().any(|b| &b.id == id) {
            return None;
        }
        self.active_branch = Some(id.clone());
        self.get(id)
    }

    /// List all branches
    pub fn list(&self) -> &[Branch] {
        &self.branches
    }

    /// List all active branches
    pub fn active_branches(&self) -> Vec<&Branch> {
        self.branches
            .iter()
            .filter(|b| b.status == BranchStatus::Active)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_branch_creation() {
        let work_dir = PathBuf::from("/tmp/test-branch");
        let branch = Branch::new("feat/test", "Implement test feature", work_dir.clone());

        assert_eq!(branch.name, "feat/test");
        assert_eq!(branch.task, "Implement test feature");
        assert_eq!(branch.work_dir, work_dir);
        assert_eq!(branch.status, BranchStatus::Active);
        assert!(branch.agent_id.is_none());
    }

    #[test]
    fn test_branch_with_agent() {
        let branch = Branch::new("feat/test", "Task", PathBuf::from("/tmp")).agent(AgentId::FORGE);

        assert_eq!(branch.agent_id, Some(AgentId::FORGE));
    }

    #[test]
    fn test_branch_registry() {
        let mut registry = BranchRegistry::new();
        let branch = Branch::new("feat/a", "Task A", PathBuf::from("/tmp/a"));
        let branch_id = branch.id.clone();

        registry.add_branch(branch);
        assert_eq!(registry.list().len(), 1);

        registry.set_active(&branch_id);
        assert_eq!(registry.active_branch, Some(branch_id.clone()));

        let removed = registry.remove(&branch_id);
        assert!(removed.is_some());
        assert!(registry.active_branch.is_none());
    }

    #[test]
    fn test_branch_info_from_branch() {
        let work_dir = PathBuf::from("/tmp/test");
        let branch = Branch::new("bug/fix", "Fix bug", work_dir);
        let info = BranchInfo::from(&branch);

        assert_eq!(info.name, "bug/fix");
        assert_eq!(info.task, "Fix bug");
        assert_eq!(info.status, BranchStatus::Active);
        assert!(!info.has_changes);
    }
}
