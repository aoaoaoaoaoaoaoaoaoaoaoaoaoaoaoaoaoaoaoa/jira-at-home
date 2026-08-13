use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

pub(crate) const ISSUES_DIR_NAME: &str = "issues";
const APP_STATE_DIR_NAME: &str = "jira_at_home";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IssueCategory {
    Feature,
    Bug,
}

impl IssueCategory {
    const ALL: [Self; 2] = [Self::Feature, Self::Bug];

    pub(crate) fn parse(raw: impl Into<String>) -> Result<Self, StoreError> {
        let raw = raw.into();
        Self::from_dir_name(raw.as_str()).ok_or(StoreError::InvalidCategory(raw))
    }

    pub(crate) fn all() -> [Self; 2] {
        Self::ALL
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Bug => "bug",
        }
    }

    fn from_dir_name(raw: &str) -> Option<Self> {
        match raw {
            "feature" => Some(Self::Feature),
            "bug" => Some(Self::Bug),
            _ => None,
        }
    }
}

impl std::fmt::Display for IssueCategory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct IssueSlug(String);

impl IssueSlug {
    pub(crate) fn parse(raw: impl Into<String>) -> Result<Self, StoreError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(StoreError::InvalidSlug("slug must not be empty".to_owned()));
        }
        if raw.starts_with('-') || raw.ends_with('-') {
            return Err(StoreError::InvalidSlug(
                "slug must not start or end with `-`".to_owned(),
            ));
        }
        if !raw
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(StoreError::InvalidSlug(
                "slug must use lowercase ascii letters, digits, and `-` only".to_owned(),
            ));
        }
        if raw.split('-').any(str::is_empty) {
            return Err(StoreError::InvalidSlug(
                "slug must not contain empty `-` segments".to_owned(),
            ));
        }
        Ok(Self(raw))
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn from_issue_path(path: &Path) -> Result<Self, StoreError> {
        let extension = path.extension().and_then(OsStr::to_str);
        if extension != Some("md") {
            return Err(StoreError::MalformedIssueEntry(
                path.display().to_string(),
                "issue file must use the `.md` extension".to_owned(),
            ));
        }
        let stem = path
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| {
                StoreError::MalformedIssueEntry(
                    path.display().to_string(),
                    "issue file name must be valid UTF-8".to_owned(),
                )
            })?
            .to_owned();
        Self::parse(stem)
    }
}

impl std::fmt::Display for IssueSlug {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct IssueKey {
    pub(crate) category: IssueCategory,
    pub(crate) slug: IssueSlug,
}

impl IssueKey {
    pub(crate) fn new(category: IssueCategory, slug: IssueSlug) -> Self {
        Self { category, slug }
    }

    fn from_issue_path(path: &Path, category: IssueCategory) -> Result<Self, StoreError> {
        Ok(Self::new(category, IssueSlug::from_issue_path(path)?))
    }
}

impl std::fmt::Display for IssueKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.category, self.slug)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IssueBody(String);

impl IssueBody {
    pub(crate) fn parse(raw: impl Into<String>) -> Result<Self, StoreError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(StoreError::EmptyIssueBody);
        }
        Ok(Self(raw))
    }

    pub(crate) fn into_inner(self) -> String {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectLayout {
    pub(crate) requested_path: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) worktree_root: PathBuf,
    pub(crate) state_identity: PathBuf,
    pub(crate) issues_root: PathBuf,
    pub(crate) state_root: PathBuf,
}

impl ProjectLayout {
    pub(crate) fn bind(requested_path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let requested_path = requested_path.into();
        let anchor = resolve_project_anchor(&requested_path)?;
        let state_root = external_state_root(&anchor.state_identity)?;
        migrate_legacy_worktree_state(&anchor, &state_root)?;
        let issues_root = state_root.join(ISSUES_DIR_NAME);
        fs::create_dir_all(&issues_root)?;
        for category in IssueCategory::all() {
            fs::create_dir_all(issues_root.join(category.as_str()))?;
        }
        fs::create_dir_all(state_root.join("mcp"))?;
        Ok(Self {
            requested_path,
            project_root: anchor.project_root,
            worktree_root: anchor.worktree_root,
            state_identity: anchor.state_identity,
            issues_root,
            state_root,
        })
    }

    pub(crate) fn issue_category_root(&self, category: IssueCategory) -> PathBuf {
        self.issues_root.join(category.as_str())
    }

    pub(crate) fn issue_path(&self, key: &IssueKey) -> PathBuf {
        self.issue_category_root(key.category)
            .join(format!("{}.md", key.slug))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectStatus {
    pub(crate) issue_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IssueSummary {
    pub(crate) key: IssueKey,
    pub(crate) path: PathBuf,
    pub(crate) updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IssueRecord {
    pub(crate) key: IssueKey,
    pub(crate) body: String,
    pub(crate) path: PathBuf,
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SaveReceipt {
    pub(crate) key: IssueKey,
    pub(crate) path: PathBuf,
    pub(crate) created: bool,
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DeleteReceipt {
    pub(crate) key: IssueKey,
    pub(crate) path: PathBuf,
    pub(crate) deleted_at: OffsetDateTime,
    pub(crate) bytes: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct IssueStore {
    layout: ProjectLayout,
}

impl IssueStore {
    pub(crate) fn bind(requested_path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        Ok(Self {
            layout: ProjectLayout::bind(requested_path)?,
        })
    }

    pub(crate) fn layout(&self) -> &ProjectLayout {
        &self.layout
    }

    pub(crate) fn status(&self) -> Result<ProjectStatus, StoreError> {
        Ok(ProjectStatus {
            issue_count: self.list()?.len(),
        })
    }

    pub(crate) fn save(&self, key: IssueKey, body: IssueBody) -> Result<SaveReceipt, StoreError> {
        let path = self.layout.issue_path(&key);
        let created = !path.exists();
        let body = body.into_inner();
        fs::write(&path, body.as_bytes())?;
        let metadata = fs::metadata(&path)?;
        Ok(SaveReceipt {
            key,
            path,
            created,
            updated_at: metadata_modified_at(&metadata.modified()?),
            bytes: body.len(),
        })
    }

    pub(crate) fn list(&self) -> Result<Vec<IssueSummary>, StoreError> {
        let mut issues = Vec::new();
        for category in IssueCategory::all() {
            for entry in fs::read_dir(self.layout.issue_category_root(category))? {
                let entry = entry?;
                let path = entry.path();
                let file_type = entry.file_type()?;
                if !file_type.is_file() {
                    return Err(StoreError::MalformedIssueEntry(
                        path.display().to_string(),
                        "issue category directory may contain only `.md` files".to_owned(),
                    ));
                }
                let key = IssueKey::from_issue_path(&path, category)?;
                let updated_at = metadata_modified_at(&entry.metadata()?.modified()?);
                issues.push(IssueSummary {
                    key,
                    path,
                    updated_at,
                });
            }
        }
        issues.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(issues)
    }

    pub(crate) fn read(&self, key: IssueKey) -> Result<IssueRecord, StoreError> {
        let path = self.layout.issue_path(&key);
        if !path.is_file() {
            return Err(StoreError::IssueNotFound(key.to_string()));
        }
        let body = fs::read_to_string(&path)?;
        let metadata = fs::metadata(&path)?;
        Ok(IssueRecord {
            key,
            bytes: body.len(),
            body,
            path,
            updated_at: metadata_modified_at(&metadata.modified()?),
        })
    }

    pub(crate) fn delete(&self, key: IssueKey) -> Result<DeleteReceipt, StoreError> {
        let path = self.layout.issue_path(&key);
        if !path.is_file() {
            return Err(StoreError::IssueNotFound(key.to_string()));
        }
        let metadata = fs::metadata(&path)?;
        let bytes = usize::try_from(metadata.len())
            .map_err(|_| StoreError::IssueTooLarge(key.to_string(), metadata.len()))?;
        fs::remove_file(&path)?;
        Ok(DeleteReceipt {
            key,
            path,
            deleted_at: OffsetDateTime::now_utc(),
            bytes,
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("issue `{0}` is too large for this platform ({1} bytes)")]
    IssueTooLarge(String, u64),
    #[error("project path `{0}` does not exist")]
    MissingProjectPath(String),
    #[error("project path `{0}` does not resolve to a directory")]
    ProjectPathNotDirectory(String),
    #[error("invalid issue category `{0}`; expected `feature` or `bug`")]
    InvalidCategory(String),
    #[error("invalid issue slug: {0}")]
    InvalidSlug(String),
    #[error("issue body must not be blank")]
    EmptyIssueBody,
    #[error("issue `{0}` does not exist")]
    IssueNotFound(String),
    #[error("malformed issue entry `{0}`: {1}")]
    MalformedIssueEntry(String, String),
    #[error("malformed git indirection `{0}`: {1}")]
    MalformedGitIndirection(String, String),
    #[error(
        "cannot migrate legacy worktree issue `{legacy_source}` to `{target}` because both exist with different bodies"
    )]
    LegacyMigrationConflict {
        legacy_source: String,
        target: String,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub(crate) fn format_timestamp(timestamp: OffsetDateTime) -> String {
    let format = &time::format_description::well_known::Rfc3339;
    timestamp
        .format(format)
        .unwrap_or_else(|_| timestamp.unix_timestamp().to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectAnchor {
    project_root: PathBuf,
    worktree_root: PathBuf,
    state_identity: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitIdentity {
    project_root: PathBuf,
    state_key: PathBuf,
}

fn resolve_project_anchor(requested_path: &Path) -> Result<ProjectAnchor, StoreError> {
    if !requested_path.exists() {
        return Err(StoreError::MissingProjectPath(
            requested_path.display().to_string(),
        ));
    }
    let canonical = requested_path.canonicalize()?;
    let search_root = if canonical.is_dir() {
        canonical
    } else {
        canonical.parent().map(Path::to_path_buf).ok_or_else(|| {
            StoreError::ProjectPathNotDirectory(requested_path.display().to_string())
        })?
    };

    for worktree_root in search_root.ancestors() {
        let git_marker = worktree_root.join(".git");
        if git_marker.exists() {
            let identity = resolve_git_identity(&git_marker, worktree_root)?;
            return Ok(ProjectAnchor {
                project_root: identity.project_root,
                worktree_root: worktree_root.to_path_buf(),
                state_identity: identity.state_key,
            });
        }
    }
    Ok(ProjectAnchor {
        project_root: search_root.clone(),
        worktree_root: search_root.clone(),
        state_identity: search_root,
    })
}

fn resolve_git_identity(
    git_marker: &Path,
    worktree_root: &Path,
) -> Result<GitIdentity, StoreError> {
    let common_dir = resolve_git_common_dir(git_marker, worktree_root)?;
    if common_dir.file_name() == Some(OsStr::new(".git")) {
        let project_root = common_dir.parent().map(Path::to_path_buf).ok_or_else(|| {
            StoreError::MalformedGitIndirection(
                common_dir.display().to_string(),
                "git common dir named `.git` has no parent".to_owned(),
            )
        })?;
        return Ok(GitIdentity {
            project_root: project_root.clone(),
            state_key: project_root,
        });
    }
    Ok(GitIdentity {
        project_root: worktree_root.to_path_buf(),
        state_key: common_dir,
    })
}

fn resolve_git_common_dir(git_marker: &Path, worktree_root: &Path) -> Result<PathBuf, StoreError> {
    let git_dir = if git_marker.is_dir() {
        git_marker.canonicalize()?
    } else if git_marker.is_file() {
        resolve_gitdir_file(git_marker, worktree_root)?
    } else {
        return Err(StoreError::MalformedGitIndirection(
            git_marker.display().to_string(),
            "git marker is neither file nor directory".to_owned(),
        ));
    };

    match fs::read_to_string(git_dir.join("commondir")) {
        Ok(raw) => resolve_git_path(&git_dir, raw.trim()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(git_dir),
        Err(error) => Err(error.into()),
    }
}

fn resolve_gitdir_file(git_marker: &Path, worktree_root: &Path) -> Result<PathBuf, StoreError> {
    let raw = fs::read_to_string(git_marker)?;
    let gitdir = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            StoreError::MalformedGitIndirection(
                git_marker.display().to_string(),
                "expected `gitdir: <path>`".to_owned(),
            )
        })?;
    resolve_git_path(worktree_root, gitdir)
}

fn resolve_git_path(base: &Path, raw: &str) -> Result<PathBuf, StoreError> {
    let path = Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Ok(path.canonicalize()?)
}

fn external_state_root(project_root: &Path) -> Result<PathBuf, StoreError> {
    let base = external_state_path(project_root);
    fs::create_dir_all(&base)?;
    Ok(base)
}

fn external_state_path(project_root: &Path) -> PathBuf {
    let mut base = dirs::state_dir().unwrap_or_else(std::env::temp_dir);
    base.push(APP_STATE_DIR_NAME);
    base.push("projects");
    for component in project_root.components() {
        match component {
            Component::Normal(part) => base.push(part),
            Component::Prefix(prefix) => base.push(prefix.as_os_str()),
            Component::CurDir | Component::ParentDir | Component::RootDir => {}
        }
    }
    base
}

fn migrate_legacy_worktree_state(
    anchor: &ProjectAnchor,
    canonical_state_root: &Path,
) -> Result<(), StoreError> {
    if anchor.worktree_root == anchor.state_identity {
        return Ok(());
    }
    let legacy_state_root = external_state_path(&anchor.worktree_root);
    if legacy_state_root == canonical_state_root || !legacy_state_root.exists() {
        return Ok(());
    }

    for category in IssueCategory::all() {
        let legacy_category = legacy_state_root
            .join(ISSUES_DIR_NAME)
            .join(category.as_str());
        if !legacy_category.exists() {
            continue;
        }
        fs::create_dir_all(
            canonical_state_root
                .join(ISSUES_DIR_NAME)
                .join(category.as_str()),
        )?;
        for entry in fs::read_dir(legacy_category)? {
            let entry = entry?;
            let source = entry.path();
            if !entry.file_type()?.is_file() {
                continue;
            }
            let key = IssueKey::from_issue_path(&source, category)?;
            let target = canonical_state_root
                .join(ISSUES_DIR_NAME)
                .join(category.as_str())
                .join(format!("{}.md", key.slug));
            migrate_legacy_issue(source.as_path(), target.as_path())?;
        }
    }
    Ok(())
}

fn migrate_legacy_issue(source: &Path, target: &Path) -> Result<(), StoreError> {
    if !target.exists() {
        fs::rename(source, target)?;
        return Ok(());
    }
    if fs::read(source)? == fs::read(target)? {
        fs::remove_file(source)?;
        return Ok(());
    }
    Err(StoreError::LegacyMigrationConflict {
        legacy_source: source.display().to_string(),
        target: target.display().to_string(),
    })
}

fn metadata_modified_at(system_time: &SystemTime) -> OffsetDateTime {
    OffsetDateTime::from(*system_time)
}
