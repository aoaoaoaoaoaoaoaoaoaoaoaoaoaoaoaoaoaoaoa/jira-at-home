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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    pub(crate) issues_root: PathBuf,
    pub(crate) state_root: PathBuf,
}

impl ProjectLayout {
    pub(crate) fn bind(requested_path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let requested_path = requested_path.into();
        let project_root = resolve_project_root(&requested_path)?;
        let state_root = external_state_root(&project_root)?;
        let issues_root = state_root.join(ISSUES_DIR_NAME);
        fs::create_dir_all(&issues_root)?;
        fs::create_dir_all(state_root.join("mcp"))?;
        Ok(Self {
            requested_path,
            project_root,
            issues_root,
            state_root,
        })
    }

    pub(crate) fn issue_path(&self, slug: &IssueSlug) -> PathBuf {
        self.issues_root.join(format!("{slug}.md"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectStatus {
    pub(crate) issue_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IssueSummary {
    pub(crate) slug: IssueSlug,
    pub(crate) updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct IssueRecord {
    pub(crate) slug: IssueSlug,
    pub(crate) body: String,
    pub(crate) path: PathBuf,
    pub(crate) updated_at: OffsetDateTime,
    pub(crate) bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SaveReceipt {
    pub(crate) slug: IssueSlug,
    pub(crate) path: PathBuf,
    pub(crate) created: bool,
    pub(crate) updated_at: OffsetDateTime,
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

    pub(crate) fn save(&self, slug: IssueSlug, body: IssueBody) -> Result<SaveReceipt, StoreError> {
        let path = self.layout.issue_path(&slug);
        let created = !path.exists();
        let body = body.into_inner();
        fs::write(&path, body.as_bytes())?;
        let metadata = fs::metadata(&path)?;
        Ok(SaveReceipt {
            slug,
            path,
            created,
            updated_at: metadata_modified_at(&metadata.modified()?),
            bytes: body.len(),
        })
    }

    pub(crate) fn list(&self) -> Result<Vec<IssueSummary>, StoreError> {
        let mut issues = Vec::new();
        for entry in fs::read_dir(&self.layout.issues_root)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if !file_type.is_file() {
                continue;
            }
            let slug = IssueSlug::from_issue_path(&path)?;
            let updated_at = metadata_modified_at(&entry.metadata()?.modified()?);
            issues.push(IssueSummary { slug, updated_at });
        }
        issues.sort_by(|left, right| left.slug.as_str().cmp(right.slug.as_str()));
        Ok(issues)
    }

    pub(crate) fn read(&self, slug: IssueSlug) -> Result<IssueRecord, StoreError> {
        let path = self.layout.issue_path(&slug);
        if !path.is_file() {
            return Err(StoreError::IssueNotFound(slug.to_string()));
        }
        let body = fs::read_to_string(&path)?;
        let metadata = fs::metadata(&path)?;
        Ok(IssueRecord {
            slug,
            bytes: body.len(),
            body,
            path,
            updated_at: metadata_modified_at(&metadata.modified()?),
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("project path `{0}` does not exist")]
    MissingProjectPath(String),
    #[error("project path `{0}` does not resolve to a directory")]
    ProjectPathNotDirectory(String),
    #[error("invalid issue slug: {0}")]
    InvalidSlug(String),
    #[error("issue body must not be blank")]
    EmptyIssueBody,
    #[error("issue `{0}` does not exist")]
    IssueNotFound(String),
    #[error("malformed issue entry `{0}`: {1}")]
    MalformedIssueEntry(String, String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub(crate) fn format_timestamp(timestamp: OffsetDateTime) -> String {
    let format = &time::format_description::well_known::Rfc3339;
    timestamp
        .format(format)
        .unwrap_or_else(|_| timestamp.unix_timestamp().to_string())
}

fn resolve_project_root(requested_path: &Path) -> Result<PathBuf, StoreError> {
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

    for ancestor in search_root.ancestors() {
        if ancestor.join(".git").exists() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Ok(search_root)
}

fn external_state_root(project_root: &Path) -> Result<PathBuf, StoreError> {
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
    fs::create_dir_all(&base)?;
    Ok(base)
}

fn metadata_modified_at(system_time: &SystemTime) -> OffsetDateTime {
    OffsetDateTime::from(*system_time)
}
