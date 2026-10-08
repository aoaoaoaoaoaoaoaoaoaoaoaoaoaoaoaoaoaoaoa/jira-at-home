//! Optional TOML front matter opening an issue body between `+++` lines.
//!
//! It names the issues that must be done first and sizes the remaining work.
//! Saving validates it strictly. Stored notes are read leniently: a note edited
//! outside the server may carry malformed front matter, which then remains
//! ordinary body text until the next save rejects it.

use serde::{Deserialize, Serialize};

use crate::store::{IssueKey, StoreError};

const FENCE: &str = "+++";

/// Remaining work in doubling units, sized against reference work rather than
/// duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct Effort(u8);

impl Effort {
    const LADDER: [u8; 6] = [1, 2, 4, 8, 16, 32];

    fn parse(raw: i64) -> Result<Self, StoreError> {
        u8::try_from(raw)
            .ok()
            .filter(|units| Self::LADDER.contains(units))
            .map(Self)
            .ok_or(StoreError::InvalidEffort(raw))
    }
}

/// Parsed front matter; empty when a body has none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct FrontMatter {
    /// Issues that must be done first. Deleting an issue removes it here.
    pub(crate) depends_on: Vec<IssueKey>,
    pub(crate) effort: Option<Effort>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<i64>,
}

impl FrontMatter {
    /// Strictly parses the front matter opening `raw` and returns the body after it.
    pub(crate) fn parse(raw: &str) -> Result<(Self, &str), StoreError> {
        let Some((header, body)) = split(raw)? else {
            return Ok((Self::default(), raw));
        };
        let fields: Fields = toml::from_str(header)
            .map_err(|error| StoreError::InvalidFrontMatter(error.message().to_owned()))?;
        let mut depends_on = Vec::with_capacity(fields.depends_on.len());
        for raw_key in &fields.depends_on {
            let key = IssueKey::parse(raw_key)
                .map_err(|_| StoreError::InvalidDependency(raw_key.clone()))?;
            if depends_on.contains(&key) {
                return Err(StoreError::DuplicateDependency(key.to_string()));
            }
            depends_on.push(key);
        }
        let effort = fields.effort.map(Effort::parse).transpose()?;
        Ok((Self { depends_on, effort }, body))
    }

    /// Reads a stored note's front matter; malformed front matter counts as body text.
    pub(crate) fn read(raw: &str) -> Self {
        Self::parse(raw).map_or_else(|_| Self::default(), |(front, _)| front)
    }

    /// Writes canonical front matter before `body`, omitting it when empty
    /// unless `body` itself opens with a fence.
    pub(crate) fn render(&self, body: &str) -> Result<String, StoreError> {
        let empty = self.depends_on.is_empty() && self.effort.is_none();
        if empty && opening_rest(body).is_none() {
            return Ok(body.to_owned());
        }
        let fields = Fields {
            depends_on: self.depends_on.iter().map(ToString::to_string).collect(),
            effort: self.effort.map(|effort| i64::from(effort.0)),
        };
        let header = toml::to_string(&fields)
            .map_err(|error| StoreError::InvalidFrontMatter(error.to_string()))?;
        Ok(format!("{FENCE}\n{header}{FENCE}\n{body}"))
    }
}

/// The text after an opening fence line, if `raw` starts with one.
fn opening_rest(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix(FENCE)?;
    rest.strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
}

/// Splits `raw` into front matter and body; `None` when `raw` has no front matter.
fn split(raw: &str) -> Result<Option<(&str, &str)>, StoreError> {
    let Some(rest) = opening_rest(raw) else {
        return Ok(None);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == FENCE {
            return Ok(Some((&rest[..offset], &rest[offset + line.len()..])));
        }
        offset += line.len();
    }
    Err(StoreError::InvalidFrontMatter(
        "missing the closing `+++` line".to_owned(),
    ))
}
