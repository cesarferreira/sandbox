use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const WORKSPACE: &str = "/workspace";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Directory mounted at /workspace: the git top level, or the cwd outside git.
    pub root: PathBuf,
    /// Where the user invoked sandbox, inside `root`.
    pub cwd: PathBuf,
    /// Shared git dir of a worktree, when it lives outside `root`.
    pub external_git_dir: Option<PathBuf>,
}

impl Project {
    pub fn detect(cwd: &Path) -> Result<Project> {
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("resolving {}", cwd.display()))?;
        let (root, common) = git_paths(&cwd).unwrap_or_else(|| (cwd.clone(), None));
        let project = Project::from_parts(root, cwd, common);
        project.check_not_too_broad(std::env::var_os("HOME").map(PathBuf::from))?;
        Ok(project)
    }

    fn from_parts(root: PathBuf, cwd: PathBuf, git_common_dir: Option<PathBuf>) -> Project {
        let external_git_dir = git_common_dir.filter(|dir| !dir.starts_with(&root));
        Project {
            root,
            cwd,
            external_git_dir,
        }
    }

    fn check_not_too_broad(&self, home: Option<PathBuf>) -> Result<()> {
        let home = home.and_then(|h| h.canonicalize().ok());
        if self.root == Path::new("/") || Some(&self.root) == home.as_ref() {
            bail!(
                "refusing to mount {} as the project; cd into a project directory first",
                self.root.display()
            );
        }
        Ok(())
    }

    /// The box's working directory, mirroring the user's position inside the project.
    pub fn workdir(&self) -> PathBuf {
        match self.cwd.strip_prefix(&self.root) {
            Ok(rel) if !rel.as_os_str().is_empty() => Path::new(WORKSPACE).join(rel),
            _ => PathBuf::from(WORKSPACE),
        }
    }

    /// A short, container-name-safe label for the project.
    pub fn slug(&self) -> String {
        let name = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let slug: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(24)
            .collect();
        let slug = slug.trim_matches('-');
        if slug.is_empty() {
            "project".into()
        } else {
            slug.into()
        }
    }
}

fn git_paths(cwd: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines().map(|l| PathBuf::from(l.trim()));
    let root = lines.next()?.canonicalize().ok()?;
    let common = lines.next().and_then(|p| p.canonicalize().ok());
    Some((root, common))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workdir_mirrors_subdirectory() {
        let p = Project::from_parts("/code/repo".into(), "/code/repo/app/src".into(), None);
        assert_eq!(p.workdir(), PathBuf::from("/workspace/app/src"));
    }

    #[test]
    fn workdir_at_root_is_workspace() {
        let p = Project::from_parts("/code/repo".into(), "/code/repo".into(), None);
        assert_eq!(p.workdir().as_os_str(), "/workspace");
    }

    #[test]
    fn git_dir_inside_root_is_not_external() {
        let p = Project::from_parts(
            "/code/repo".into(),
            "/code/repo".into(),
            Some("/code/repo/.git".into()),
        );
        assert_eq!(p.external_git_dir, None);
    }

    #[test]
    fn worktree_git_dir_is_external() {
        let p = Project::from_parts(
            "/code/repo-wt".into(),
            "/code/repo-wt".into(),
            Some("/code/repo/.git".into()),
        );
        assert_eq!(p.external_git_dir, Some(PathBuf::from("/code/repo/.git")));
    }

    #[test]
    fn refuses_root_and_home() {
        let root = Project::from_parts("/".into(), "/".into(), None);
        assert!(root.check_not_too_broad(None).is_err());

        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let home = Project::from_parts(tmp.clone(), tmp.clone(), None);
        assert!(home.check_not_too_broad(Some(tmp)).is_err());
    }

    #[test]
    fn slug_is_container_safe() {
        let p = Project::from_parts("/x/My Project_v2".into(), "/x/My Project_v2".into(), None);
        assert_eq!(p.slug(), "my-project-v2");
    }
}
