use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;

use crate::front_matter::FrontMatter;

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

/// Shared shape rule for every path-segment identifier (issue slugs, epic
/// names): lowercase ascii letters, digits, and single `-` separators.
fn validate_segment_shape(raw: &str) -> Result<(), &'static str> {
    if raw.is_empty() {
        return Err("must not be empty");
    }
    if raw.starts_with('-') || raw.ends_with('-') {
        return Err("must not start or end with `-`");
    }
    if !raw
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("must use lowercase ascii letters, digits, and `-` only");
    }
    if raw.split('-').any(str::is_empty) {
        return Err("must not contain empty `-` segments");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct IssueSlug(String);

impl IssueSlug {
    pub(crate) fn parse(raw: impl Into<String>) -> Result<Self, StoreError> {
        let raw = raw.into();
        validate_segment_shape(&raw)
            .map_err(|reason| StoreError::InvalidSlug(format!("slug {reason}")))?;
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

/// An epic is nothing but its subdirectory name: `issues/<category>/<epic>/<slug>.md`.
/// It carries no metadata of its own and exists only while it contains an issue.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct Epic(String);

impl Epic {
    pub(crate) fn parse(raw: impl Into<String>) -> Result<Self, StoreError> {
        let raw = raw.into();
        validate_segment_shape(&raw)
            .map_err(|reason| StoreError::InvalidEpic(format!("epic {reason}")))?;
        Ok(Self(raw))
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }

    fn from_dir_name(name: &OsStr) -> Result<Self, StoreError> {
        let name = name.to_str().ok_or_else(|| {
            StoreError::MalformedIssueEntry(
                name.to_string_lossy().into_owned(),
                "epic directory name must be valid UTF-8".to_owned(),
            )
        })?;
        Self::parse(name)
    }
}

impl std::fmt::Display for Epic {
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

    /// Parses the `category/slug` form that `Display` writes.
    pub(crate) fn parse(raw: &str) -> Result<Self, StoreError> {
        let (category, slug) = raw
            .split_once('/')
            .ok_or_else(|| StoreError::InvalidDependency(raw.to_owned()))?;
        Ok(Self::new(
            IssueCategory::parse(category)?,
            IssueSlug::parse(slug)?,
        ))
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

/// A note as stored: Markdown, optionally opened by validated front matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueBody {
    raw: String,
    front: FrontMatter,
}

impl IssueBody {
    pub(crate) fn parse(raw: impl Into<String>) -> Result<Self, StoreError> {
        let raw = raw.into();
        let (front, body) = FrontMatter::parse(&raw)?;
        if body.trim().is_empty() {
            return Err(StoreError::EmptyIssueBody);
        }
        Ok(Self { raw, front })
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
    pub(crate) fn resolve(requested_path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let requested_path = requested_path.into();
        let anchor = resolve_project_anchor(&requested_path)?;
        let state_root = external_state_path(&anchor.state_identity);
        let issues_root = state_root.join(ISSUES_DIR_NAME);
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

    pub(crate) fn epic_dir(&self, category: IssueCategory, epic: &Epic) -> PathBuf {
        self.issue_category_root(category).join(epic.as_str())
    }

    pub(crate) fn issue_path(&self, key: &IssueKey, epic: Option<&Epic>) -> PathBuf {
        let dir = epic.map_or_else(
            || self.issue_category_root(key.category),
            |epic| self.epic_dir(key.category, epic),
        );
        dir.join(format!("{}.md", key.slug))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IssueSummary {
    pub(crate) key: IssueKey,
    pub(crate) epic: Option<Epic>,
    pub(crate) path: PathBuf,
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) front: FrontMatter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IssueRecord {
    pub(crate) key: IssueKey,
    pub(crate) epic: Option<Epic>,
    pub(crate) body: String,
    pub(crate) path: PathBuf,
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) bytes: usize,
    /// Issues whose front matter lists this one.
    pub(crate) dependents: Vec<IssueKey>,
}

/// What `IssueStore::save` did to reach the target location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SaveOutcome {
    /// No issue with this slug existed anywhere in the category.
    Created,
    /// An issue already sat at exactly this location; its body was replaced.
    Updated,
    /// An issue with this slug existed at a different epic location and was
    /// relocated here.
    Moved,
}

impl SaveOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Moved => "moved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SaveReceipt {
    pub(crate) key: IssueKey,
    pub(crate) epic: Option<Epic>,
    pub(crate) path: PathBuf,
    pub(crate) outcome: SaveOutcome,
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) bytes: usize,
    pub(crate) front: FrontMatter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DeleteReceipt {
    pub(crate) key: IssueKey,
    pub(crate) epic: Option<Epic>,
    pub(crate) path: PathBuf,
    pub(crate) deleted_at: OffsetDateTime,
    pub(crate) bytes: usize,
    /// Issues that listed the deleted one and no longer do.
    pub(crate) dependents: Vec<IssueKey>,
}

#[derive(Debug, Clone)]
pub(crate) struct IssueStore {
    layout: ProjectLayout,
}

impl IssueStore {
    pub(crate) fn open(layout: ProjectLayout) -> Result<Self, StoreError> {
        migrate_legacy_worktree_state(&layout)?;
        Ok(Self { layout })
    }

    pub(crate) fn layout(&self) -> &ProjectLayout {
        &self.layout
    }

    /// Save `body` under `epic` (or uncategorized, if `None`). A slug is
    /// unique within its category across every epic: if it already exists
    /// elsewhere in the category, that file is relocated here and its old
    /// epic directory is pruned once empty.
    pub(crate) fn save(
        &self,
        key: IssueKey,
        epic: Option<Epic>,
        body: IssueBody,
    ) -> Result<SaveReceipt, StoreError> {
        self.validate_dependencies(&key, &body.front)?;
        let target_path = self.layout.issue_path(&key, epic.as_ref());
        let existing = self.locate_slug(key.category, &key.slug)?;
        fs::create_dir_all(
            target_path
                .parent()
                .ok_or_else(|| io::Error::other("issue path has no parent directory"))?,
        )?;
        let IssueBody { raw, front } = body;
        fs::write(&target_path, raw.as_bytes())?;

        let outcome = match existing {
            None => SaveOutcome::Created,
            Some((old_path, _)) if old_path == target_path => SaveOutcome::Updated,
            Some((old_path, old_epic)) => {
                fs::remove_file(&old_path)?;
                if let Some(old_epic) = old_epic {
                    remove_epic_dir_if_empty(&self.layout.epic_dir(key.category, &old_epic))?;
                }
                SaveOutcome::Moved
            }
        };

        let metadata = fs::metadata(&target_path)?;
        Ok(SaveReceipt {
            key,
            epic,
            path: target_path,
            outcome,
            updated_at: metadata_modified_at(&metadata.modified()?),
            bytes: raw.len(),
            front,
        })
    }

    /// Dependencies must name other existing issues and must not close a cycle.
    fn validate_dependencies(&self, key: &IssueKey, front: &FrontMatter) -> Result<(), StoreError> {
        if front.depends_on.is_empty() {
            return Ok(());
        }
        let issues = self.list()?;
        let graph = issues
            .iter()
            .map(|issue| (&issue.key, issue.front.depends_on.as_slice()))
            .collect::<BTreeMap<_, _>>();
        for dependency in &front.depends_on {
            if dependency == key {
                return Err(StoreError::SelfDependency(key.to_string()));
            }
            if !graph.contains_key(dependency) {
                return Err(StoreError::UnknownDependency(dependency.to_string()));
            }
            let mut pending = vec![dependency];
            let mut visited = BTreeSet::new();
            while let Some(current) = pending.pop() {
                if current == key {
                    return Err(StoreError::DependencyCycle {
                        dependency: dependency.to_string(),
                        issue: key.to_string(),
                    });
                }
                if visited.insert(current) {
                    pending.extend(graph.get(current).copied().unwrap_or_default());
                }
            }
        }
        Ok(())
    }

    pub(crate) fn list(&self) -> Result<Vec<IssueSummary>, StoreError> {
        let mut issues = Vec::new();
        for category in IssueCategory::all() {
            collect_category_issues(
                &self.layout.issue_category_root(category),
                category,
                None,
                &mut issues,
            )?;
        }
        issues.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(issues)
    }

    pub(crate) fn read(&self, key: IssueKey) -> Result<IssueRecord, StoreError> {
        let (path, epic) = self
            .locate_slug(key.category, &key.slug)?
            .ok_or_else(|| StoreError::IssueNotFound(key.to_string()))?;
        let body = fs::read_to_string(&path)?;
        let metadata = fs::metadata(&path)?;
        let dependents = self
            .list()?
            .into_iter()
            .filter(|issue| issue.front.depends_on.contains(&key))
            .map(|issue| issue.key)
            .collect();
        Ok(IssueRecord {
            key,
            epic,
            bytes: body.len(),
            body,
            path,
            updated_at: metadata_modified_at(&metadata.modified()?),
            dependents,
        })
    }

    /// Deleting means done: the deleted issue leaves every `depends_on` that listed it.
    pub(crate) fn delete(&self, key: IssueKey) -> Result<DeleteReceipt, StoreError> {
        let (path, epic) = self
            .locate_slug(key.category, &key.slug)?
            .ok_or_else(|| StoreError::IssueNotFound(key.to_string()))?;
        let metadata = fs::metadata(&path)?;
        let bytes = usize::try_from(metadata.len())
            .map_err(|_| StoreError::IssueTooLarge(key.to_string(), metadata.len()))?;
        fs::remove_file(&path)?;
        if let Some(epic) = &epic {
            remove_epic_dir_if_empty(&self.layout.epic_dir(key.category, epic))?;
        }
        let mut dependents = Vec::new();
        for issue in self.list()? {
            if !issue.front.depends_on.contains(&key) {
                continue;
            }
            let raw = fs::read_to_string(&issue.path)?;
            let (mut front, body) = FrontMatter::parse(&raw)?;
            front.depends_on.retain(|dependency| dependency != &key);
            fs::write(&issue.path, front.render(body)?)?;
            dependents.push(issue.key);
        }
        Ok(DeleteReceipt {
            key,
            epic,
            path,
            deleted_at: OffsetDateTime::now_utc(),
            bytes,
            dependents,
        })
    }

    /// Find an existing issue by slug anywhere in `category`: directly, or
    /// inside exactly one epic subdirectory. A slug is unique within its
    /// category, so the first match is the only match.
    fn locate_slug(
        &self,
        category: IssueCategory,
        slug: &IssueSlug,
    ) -> Result<Option<(PathBuf, Option<Epic>)>, StoreError> {
        let root = self.layout.issue_category_root(category);
        let direct = root.join(format!("{slug}.md"));
        if direct.is_file() {
            return Ok(Some((direct, None)));
        }
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let epic = Epic::from_dir_name(&entry.file_name())?;
            let candidate = entry.path().join(format!("{slug}.md"));
            if candidate.is_file() {
                return Ok(Some((candidate, Some(epic))));
            }
        }
        Ok(None)
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
    #[error("invalid epic: {0}")]
    InvalidEpic(String),
    #[error("issue body must not be blank")]
    EmptyIssueBody,
    #[error("invalid front matter: {0}")]
    InvalidFrontMatter(String),
    #[error("invalid dependency `{0}`; expected `category/slug`")]
    InvalidDependency(String),
    #[error("effort must be 1, 2, 4, 8, 16 or 32, not {0}")]
    InvalidEffort(i64),
    #[error("dependency `{0}` is listed twice")]
    DuplicateDependency(String),
    #[error("issue `{0}` cannot depend on itself")]
    SelfDependency(String),
    #[error("dependency `{0}` does not exist")]
    UnknownDependency(String),
    #[error("dependency cycle: `{dependency}` already depends on `{issue}`")]
    DependencyCycle { dependency: String, issue: String },
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

/// Walk `dir` (a category root, or one epic subdirectory of one) collecting
/// every issue file. Epics nest exactly one level deep under a category.
fn collect_category_issues(
    dir: &Path,
    category: IssueCategory,
    epic: Option<&Epic>,
    out: &mut Vec<IssueSummary>,
) -> Result<(), StoreError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if epic.is_some() {
                return Err(StoreError::MalformedIssueEntry(
                    path.display().to_string(),
                    "epic directories must not nest".to_owned(),
                ));
            }
            let nested_epic = Epic::from_dir_name(&entry.file_name())?;
            collect_category_issues(&path, category, Some(&nested_epic), out)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(StoreError::MalformedIssueEntry(
                path.display().to_string(),
                "issue category directory may contain only `.md` files and epic subdirectories"
                    .to_owned(),
            ));
        }
        let key = IssueKey::from_issue_path(&path, category)?;
        let updated_at = metadata_modified_at(&entry.metadata()?.modified()?);
        let front = FrontMatter::read(&fs::read_to_string(&path)?);
        out.push(IssueSummary {
            key,
            epic: epic.cloned(),
            path,
            updated_at,
            front,
        });
    }
    Ok(())
}

/// Remove `dir` once it holds no more issues; an epic is only its directory.
fn remove_epic_dir_if_empty(dir: &Path) -> Result<(), StoreError> {
    if fs::read_dir(dir)?.next().is_none() {
        fs::remove_dir(dir)?;
    }
    Ok(())
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
        if git_marker.exists() && (!git_marker.is_dir() || git_marker.join("HEAD").is_file()) {
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
    let target = worktree_root.join(gitdir);
    if !target.join("HEAD").is_file() {
        return Err(StoreError::MalformedGitIndirection(
            git_marker.display().to_string(),
            "gitdir target has no HEAD".to_owned(),
        ));
    }
    Ok(target.canonicalize()?)
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

pub(crate) fn state_base() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_STATE_DIR_NAME)
}

fn external_state_path(project_root: &Path) -> PathBuf {
    let mut base = state_base().join("projects");
    for component in project_root.components() {
        match component {
            Component::Normal(part) => base.push(part),
            Component::Prefix(prefix) => base.push(prefix.as_os_str()),
            Component::CurDir | Component::ParentDir | Component::RootDir => {}
        }
    }
    base
}

fn migrate_legacy_worktree_state(layout: &ProjectLayout) -> Result<(), StoreError> {
    let canonical_state_root = layout.state_root.as_path();
    if layout.worktree_root == layout.state_identity {
        return Ok(());
    }
    let legacy_state_root = external_state_path(&layout.worktree_root);
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
