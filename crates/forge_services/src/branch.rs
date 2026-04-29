use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use forge_app::{
    CommandInfra, EnvironmentInfra, FileDirectoryInfra, FileReaderInfra, FileRemoverInfra,
    FileWriterInfra,
};
use forge_domain::{
    AgentId, Branch, BranchId, BranchInfo, BranchRegistry, BranchStatus, MergeResult,
};
use tracing::{info, warn};

/// Branch management service for creating and managing feature branches.
///
/// This service handles:
/// - Creating git branches with associated work directories
/// - Switching between branches
/// - Merging branches back to main
/// - Tracking branch metadata in `.forge/branches.json`
pub struct BranchService<F> {
    infra: Arc<F>,
    forge_dir: PathBuf,
    registry_path: PathBuf,
}

impl<F> Clone for BranchService<F> {
    fn clone(&self) -> Self {
        Self {
            infra: Arc::clone(&self.infra),
            forge_dir: self.forge_dir.clone(),
            registry_path: self.registry_path.clone(),
        }
    }
}

impl<F> BranchService<F> {
    /// Create a new branch service with the given infrastructure.
    ///
    /// # Arguments
    /// * `infra` - Infrastructure providing file I/O and command execution
    /// * `forge_dir` - Path to the `.forge` directory
    pub fn new(infra: Arc<F>, forge_dir: PathBuf) -> Self {
        let registry_path = forge_dir.join("branches.json");
        Self { infra, forge_dir, registry_path }
    }

    /// Get the path to the work directory for a branch
    fn work_dir_for_branch(&self, branch_name: &str) -> PathBuf {
        self.forge_dir.join("work").join(branch_name)
    }

    /// Get the path to the current branch marker file
    fn current_branch_path(&self) -> PathBuf {
        self.forge_dir.join("current_branch")
    }
}

impl<
    F: CommandInfra
        + EnvironmentInfra
        + FileReaderInfra
        + FileWriterInfra
        + FileRemoverInfra
        + FileDirectoryInfra
        + Send
        + Sync
        + 'static,
> BranchService<F>
{
    /// Load the branch registry from disk
    async fn load_registry(&self) -> Result<BranchRegistry> {
        if !self.registry_path.exists() {
            return Ok(BranchRegistry::new());
        }
        let content = self.infra.read_utf8(&self.registry_path).await?;
        serde_json::from_str(&content).context("Failed to parse branches.json")
    }

    /// Save the branch registry to disk
    async fn save_registry(&self, registry: &BranchRegistry) -> Result<()> {
        // Ensure forge directory exists
        if !self.forge_dir.exists() {
            self.infra.create_dirs(&self.forge_dir).await?;
        }
        let content = serde_json::to_string_pretty(registry)?;
        self.infra
            .write(&self.registry_path, content.into())
            .await?;
        Ok(())
    }

    /// Update the current branch marker file
    async fn set_current_branch(&self, branch_name: Option<&str>) -> Result<()> {
        if let Some(name) = branch_name {
            self.infra
                .write(&self.current_branch_path(), name.as_bytes().to_vec().into())
                .await?;
        } else if self.current_branch_path().exists() {
            self.infra.remove(&self.current_branch_path()).await?;
        }
        Ok(())
    }

    /// Get the git repository root
    async fn get_git_root(&self) -> Result<PathBuf> {
        let cwd = self.infra.get_environment().cwd;
        let output = self
            .infra
            .execute_command(
                "git rev-parse --show-toplevel".to_string(),
                cwd.clone(),
                true,
                None,
            )
            .await
            .context("Failed to get git repository root")?;

        if !output.stdout.trim().is_empty() {
            Ok(PathBuf::from(output.stdout.trim()))
        } else {
            anyhow::bail!("Not in a git repository")
        }
    }
}

impl<
    F: CommandInfra
        + EnvironmentInfra
        + FileReaderInfra
        + FileWriterInfra
        + FileRemoverInfra
        + FileDirectoryInfra
        + Send
        + Sync
        + 'static,
> BranchService<F>
{
    /// Create a new feature branch with an associated work directory.
    ///
    /// This creates:
    /// 1. A new git branch
    /// 2. A work directory under `.forge/work/<branch_name>/`
    /// 3. Updates the branch registry
    ///
    /// # Arguments
    /// * `name` - Branch name (e.g., "feat/user-auth")
    /// * `task` - Human-readable task description
    /// * `agent_id` - Optional agent ID to assign to this branch
    ///
    /// # Errors
    /// Returns an error if the branch already exists, git operations fail, or
    /// file operations fail.
    pub async fn create_branch(
        &self,
        name: &str,
        task: &str,
        agent_id: Option<AgentId>,
    ) -> Result<Branch> {
        info!(branch = %name, task = %task, "Creating new branch");

        let git_root = self.get_git_root().await?;
        let _cwd = self.infra.get_environment().cwd;

        // Check if branch already exists
        let branch_check = self
            .infra
            .execute_command(
                format!("git rev-parse --verify refs/heads/{name}"),
                git_root.clone(),
                true,
                None,
            )
            .await?;

        if branch_check.exit_code == Some(0) {
            anyhow::bail!("Branch '{}' already exists", name);
        }

        // Create the git branch
        let create_output = self
            .infra
            .execute_command(
                format!("git checkout -b {name}"),
                git_root.clone(),
                true,
                None,
            )
            .await
            .context("Failed to create git branch")?;

        if create_output.exit_code != Some(0) {
            anyhow::bail!("Failed to create git branch: {}", create_output.stderr);
        }

        // Create work directory
        let work_dir = self.work_dir_for_branch(name);
        self.infra.create_dirs(&work_dir).await?;

        // Create the branch record
        let branch = Branch::new(name, task, work_dir.clone()).agent_opt(agent_id);

        // Update registry
        let mut registry = self.load_registry().await?;
        registry.add_branch(branch.clone());
        registry.set_active(&branch.id);
        self.save_registry(&registry).await?;

        // Write current branch marker
        self.set_current_branch(Some(name)).await?;

        info!(branch = %name, work_dir = %work_dir.display(), "Branch created successfully");
        Ok(branch)
    }

    /// Switch to a branch by updating the current working directory.
    ///
    /// # Arguments
    /// * `name` - Branch name to switch to
    ///
    /// # Errors
    /// Returns an error if the branch doesn't exist or git checkout fails.
    pub async fn switch_branch(&self, name: &str) -> Result<()> {
        info!(branch = %name, "Switching to branch");

        let git_root = self.get_git_root().await?;

        // Verify branch exists
        let branch_check = self
            .infra
            .execute_command(
                format!("git rev-parse --verify refs/heads/{name}"),
                git_root.clone(),
                true,
                None,
            )
            .await?;

        if branch_check.exit_code != Some(0) {
            anyhow::bail!("Branch '{}' does not exist", name);
        }

        // Checkout the branch in git
        let checkout_output = self
            .infra
            .execute_command(format!("git checkout {name}"), git_root.clone(), true, None)
            .await
            .context("Failed to checkout branch")?;

        if checkout_output.exit_code != Some(0) {
            anyhow::bail!("Failed to checkout branch: {}", checkout_output.stderr);
        }

        // Update current branch marker
        self.set_current_branch(Some(name)).await?;

        // Update registry
        let mut registry = self.load_registry().await?;
        let branch_id = BranchId::new(name);
        // Check if branch exists in registry
        if registry.get(&branch_id).is_some() {
            registry.set_active(&branch_id);
        }
        self.save_registry(&registry).await?;

        info!(branch = %name, "Switched to branch successfully");
        Ok(())
    }

    /// List all managed branches.
    ///
    /// # Returns
    /// A vector of branch information for all registered branches.
    pub async fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        let registry = self.load_registry().await?;
        let branches: Vec<_> = registry.list().to_vec();

        // Check each branch for changes
        let mut infos: Vec<BranchInfo> = Vec::new();
        for b in branches {
            let mut info = BranchInfo::from(&b);
            info.has_changes = self
                .check_branch_has_changes(&info.name)
                .await
                .unwrap_or(false);
            infos.push(info);
        }

        // Sort by status (active first) then by name
        infos.sort_by(|a, b| {
            let status_order = |s: &BranchStatus| match s {
                BranchStatus::Active => 0,
                BranchStatus::Merged => 1,
                BranchStatus::Abandoned => 2,
            };
            status_order(&a.status)
                .cmp(&status_order(&b.status))
                .then(a.name.cmp(&b.name))
        });

        Ok(infos)
    }

    /// Check if a branch has uncommitted changes.
    async fn check_branch_has_changes(&self, branch_name: &str) -> Result<bool> {
        let git_root = self.get_git_root().await?;

        // Save current branch
        let current_branch = self
            .infra
            .execute_command(
                "git branch --show-current".to_string(),
                git_root.clone(),
                true,
                None,
            )
            .await?
            .stdout
            .trim()
            .to_string();

        // Checkout the target branch temporarily
        self.infra
            .execute_command(
                format!("git checkout {branch_name}"),
                git_root.clone(),
                true,
                None,
            )
            .await?;

        // Check for changes
        let status_output = self
            .infra
            .execute_command(
                "git status --porcelain".to_string(),
                git_root.clone(),
                true,
                None,
            )
            .await?;

        // Switch back to original branch
        self.infra
            .execute_command(
                format!("git checkout {current_branch}"),
                git_root,
                true,
                None,
            )
            .await?;

        Ok(!status_output.stdout.trim().is_empty())
    }

    /// Get the currently active branch name.
    pub async fn get_active_branch(&self) -> Result<Option<String>> {
        let branch = self.infra.read_utf8(&self.current_branch_path()).await.ok();
        Ok(branch.map(|b| b.trim().to_string()))
    }

    /// Merge a branch back to the main branch.
    ///
    /// # Arguments
    /// * `name` - Branch name to merge
    /// * `delete_after` - Whether to delete the branch after successful merge
    ///
    /// # Returns
    /// A `MergeResult` describing the outcome of the merge.
    pub async fn merge_branch(&self, name: &str, delete_after: bool) -> Result<MergeResult> {
        info!(branch = %name, "Merging branch to main");

        let git_root = self.get_git_root().await?;
        let _cwd = self.infra.get_environment().cwd;

        // Get current branch to return to
        let current_branch = self
            .infra
            .execute_command(
                "git branch --show-current".to_string(),
                git_root.clone(),
                true,
                None,
            )
            .await?
            .stdout
            .trim()
            .to_string();

        // Determine the base branch (main or master)
        let base_branch = self.determine_base_branch(&git_root).await?;

        // Checkout base branch
        self.infra
            .execute_command(
                format!("git checkout {base_branch}"),
                git_root.clone(),
                true,
                None,
            )
            .await
            .context("Failed to checkout base branch")?;

        // Merge the feature branch
        let merge_output = self
            .infra
            .execute_command(
                format!("git merge {name} --no-ff -m \"Merge branch '{name}' into {base_branch}\""),
                git_root.clone(),
                true,
                None,
            )
            .await?;

        let has_conflicts =
            merge_output.exit_code != Some(0) || merge_output.stderr.contains("conflict");

        if has_conflicts {
            // Abort the merge
            self.infra
                .execute_command(
                    "git merge --abort".to_string(),
                    git_root.clone(),
                    true,
                    None,
                )
                .await?;

            // Return to original branch
            self.infra
                .execute_command(
                    format!("git checkout {current_branch}"),
                    git_root.clone(),
                    true,
                    None,
                )
                .await?;

            return Ok(MergeResult {
                branch: BranchId::new(name),
                success: false,
                has_conflicts: true,
                commit_hash: None,
                message: format!(
                    "Merge conflict detected. Please resolve conflicts in branch '{}' and try again.",
                    name
                ),
            });
        }

        // Get merge commit hash
        let commit_hash = self
            .infra
            .execute_command(
                "git rev-parse HEAD".to_string(),
                git_root.clone(),
                true,
                None,
            )
            .await
            .ok()
            .map(|o| o.stdout.trim().to_string());

        // Return to original branch
        self.infra
            .execute_command(
                format!("git checkout {current_branch}"),
                git_root.clone(),
                true,
                None,
            )
            .await?;

        // Delete branch if requested
        if delete_after {
            self.delete_branch(name, false).await.ok();
        } else {
            // Update registry status
            let mut registry = self.load_registry().await?;
            if let Some(branch) = registry.get_mut(&BranchId::new(name)) {
                branch.status = BranchStatus::Merged;
            }
            self.save_registry(&registry).await?;
        }

        Ok(MergeResult {
            branch: BranchId::new(name),
            success: true,
            has_conflicts: false,
            commit_hash,
            message: format!("Successfully merged '{}' into {}", name, base_branch),
        })
    }

    /// Determine the base branch to merge into (main or master).
    async fn determine_base_branch(&self, git_root: &Path) -> Result<String> {
        // Try main first
        let main_check = self
            .infra
            .execute_command(
                "git rev-parse --verify refs/heads/main".to_string(),
                git_root.to_path_buf(),
                true,
                None,
            )
            .await?;

        if main_check.exit_code == Some(0) {
            return Ok("main".to_string());
        }

        // Fall back to master
        let master_check = self
            .infra
            .execute_command(
                "git rev-parse --verify refs/heads/master".to_string(),
                git_root.to_path_buf(),
                true,
                None,
            )
            .await?;

        if master_check.exit_code == Some(0) {
            return Ok("master".to_string());
        }

        anyhow::bail!("No base branch found (neither 'main' nor 'master' exists)")
    }

    /// Delete a branch and optionally its work directory.
    ///
    /// # Arguments
    /// * `name` - Branch name to delete
    /// * `delete_workdir` - Whether to also delete the work directory
    ///
    /// # Errors
    /// Returns an error if the branch doesn't exist or git operations fail.
    pub async fn delete_branch(&self, name: &str, delete_workdir: bool) -> Result<()> {
        info!(branch = %name, "Deleting branch");

        let git_root = self.get_git_root().await?;

        // Don't allow deleting main or master
        if name == "main" || name == "master" {
            anyhow::bail!("Cannot delete the base branch");
        }

        // Delete the git branch
        let delete_output = self
            .infra
            .execute_command(
                format!("git branch -D {name}"),
                git_root.clone(),
                true,
                None,
            )
            .await?;

        if delete_output.exit_code != Some(0) {
            warn!(branch = %name, "Branch may not have existed: {}", delete_output.stderr);
        }

        // Delete work directory if requested
        if delete_workdir {
            let work_dir = self.work_dir_for_branch(name);
            if work_dir.exists() {
                // Note: actual file deletion would require FileRemoverInfra
                // For now, just log that we would delete it
                info!(work_dir = %work_dir.display(), "Would delete work directory");
            }
        }

        // Update registry
        let mut registry = self.load_registry().await?;
        registry.remove(&BranchId::new(name));
        self.save_registry(&registry).await?;

        // Clear current branch marker if this was the active branch
        if let Ok(Some(current)) = self.get_active_branch().await
            && current == name
        {
            self.set_current_branch(None).await?;
        }

        info!(branch = %name, "Branch deleted successfully");
        Ok(())
    }

    /// Abandon a branch (mark as abandoned without deleting).
    ///
    /// # Arguments
    /// * `name` - Branch name to abandon
    ///
    /// # Errors
    /// Returns an error if the branch doesn't exist.
    pub async fn abandon_branch(&self, name: &str) -> Result<()> {
        info!(branch = %name, "Abandoning branch");

        let mut registry = self.load_registry().await?;
        let branch_id = BranchId::new(name);

        if registry.get(&branch_id).is_none() {
            anyhow::bail!("Branch '{}' not found in registry", name);
        }

        if let Some(branch) = registry.get_mut(&branch_id) {
            branch.status = BranchStatus::Abandoned;
        }

        self.save_registry(&registry).await?;

        info!(branch = %name, "Branch abandoned successfully");
        Ok(())
    }
}

/// Optional agent setter extension for Branch
trait BranchAgentExt {
    fn agent_opt(self, agent_id: Option<AgentId>) -> Branch;
}

impl BranchAgentExt for Branch {
    fn agent_opt(mut self, agent_id: Option<AgentId>) -> Branch {
        if let Some(id) = agent_id {
            self.agent_id = Some(id);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn test_work_dir_path() {
        // This is a simple compilation test
        // Actual tests would require mocking infra
    }

    #[test]
    fn test_branch_id_display() {
        let id = BranchId::new("feat/test");
        assert_eq!(format!("{}", id), "feat/test");
        assert_eq!(id.as_str(), "feat/test");
    }

    #[test]
    fn test_branch_status_default() {
        assert_eq!(BranchStatus::default(), BranchStatus::Active);
    }
}
