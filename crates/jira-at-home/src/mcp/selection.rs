use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Default, Deserialize)]
pub(crate) struct PageArgs {
    #[serde(default)]
    limit: Option<PageLimit>,
    #[serde(default)]
    offset: usize,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(try_from = "usize")]
struct PageLimit(usize);

impl TryFrom<usize> for PageLimit {
    type Error = &'static str;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        if (1..=200).contains(&value) {
            Ok(Self(value))
        } else {
            Err("limit must be between 1 and 200")
        }
    }
}

impl PageArgs {
    pub(crate) fn select<'a, T>(&self, items: &'a [T], default_limit: usize) -> Page<'a, T> {
        let start = self.offset.min(items.len());
        let end = start
            .saturating_add(self.limit.map_or(default_limit, |limit| limit.0))
            .min(items.len());
        Page {
            items: &items[start..end],
            count: items.len(),
            offset: self.offset,
            next_offset: (end < items.len()).then_some(end),
        }
    }
}

pub(crate) struct Page<'a, T> {
    pub(crate) items: &'a [T],
    count: usize,
    offset: usize,
    next_offset: Option<usize>,
}

impl<T> Page<'_, T> {
    pub(crate) fn metadata(&self) -> Value {
        json!({"count": self.count, "offset": self.offset, "next_offset": self.next_offset})
    }
}
