use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_LIST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledResourcePack {
    pub theme_id: String,
    pub name: String,
    pub version: Option<String>,
    pub version_code: Option<u64>,
    pub author: Option<String>,
    pub metadata_status: String,
}

impl InstalledResourcePack {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            crate::device::crpack::valid_theme_id(&self.theme_id),
            "invalid listed themeId"
        );
        ensure!(
            !self.name.is_empty() && self.name.len() <= 128,
            "invalid listed name"
        );
        ensure!(
            self.version.as_ref().is_none_or(|v| v.len() <= 64),
            "invalid listed version"
        );
        ensure!(
            self.author.as_ref().is_none_or(|v| v.len() <= 128),
            "invalid listed author"
        );
        ensure!(
            self.version_code.is_none_or(|v| v <= MAX_SAFE_INTEGER),
            "invalid listed versionCode"
        );
        match self.metadata_status.as_str() {
            "ok" => Ok(()),
            "unavailable" => {
                ensure!(
                    self.name == self.theme_id
                        && self.version.is_none()
                        && self.version_code.is_none()
                        && self.author.is_none(),
                    "unavailable list metadata must use fallback fields"
                );
                Ok(())
            }
            _ => bail!("invalid metadataStatus"),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPage {
    page_index: usize,
    done: bool,
    total: usize,
    items: Vec<InstalledResourcePack>,
}

pub(super) struct ListCollector {
    request_id: String,
    pages: BTreeMap<usize, ListPage>,
    total: Option<usize>,
    last_page: Option<usize>,
    bytes: usize,
}

impl ListCollector {
    pub(super) fn new(request_id: String) -> Self {
        Self {
            request_id,
            pages: BTreeMap::new(),
            total: None,
            last_page: None,
            bytes: 0,
        }
    }

    pub(super) fn push(
        &mut self,
        value: Value,
    ) -> anyhow::Result<Option<Vec<InstalledResourcePack>>> {
        if value["replyTo"].as_str() != Some(self.request_id.as_str()) {
            return Ok(None);
        }
        if let Some(code) = value.get("errorCode") {
            bail!(
                "resource pack list: {}",
                code.as_str().unwrap_or("invalid error response")
            );
        }
        let bytes = serde_json::to_vec(&value)?.len();
        let page: ListPage =
            serde_json::from_value(value).context("invalid resource pack list page")?;
        ensure!(
            self.total.is_none_or(|total| total == page.total),
            "list total changed between pages"
        );
        ensure!(page.items.len() <= page.total, "list page exceeds total");
        ensure!(
            (page.total == 0 && page.page_index == 0 && page.done && page.items.is_empty())
                || (page.total > 0 && page.page_index < page.total && !page.items.is_empty()),
            "invalid empty list page or pageIndex"
        );
        for item in &page.items {
            item.validate()?;
        }
        if let Some(previous) = self.pages.get(&page.page_index) {
            ensure!(previous == &page, "conflicting duplicate list page");
            return Ok(None);
        }
        ensure!(
            self.bytes
                .checked_add(bytes)
                .is_some_and(|n| n <= MAX_LIST_BYTES),
            "resource pack list exceeds local memory budget"
        );
        if let Some(last) = self.last_page {
            ensure!(
                page.page_index <= last && (!page.done || page.page_index == last),
                "list page after final page"
            );
        }
        if page.done {
            ensure!(
                self.pages.keys().all(|index| *index < page.page_index),
                "final page precedes collected pages"
            );
            self.last_page = Some(page.page_index);
        }
        self.total = Some(page.total);
        self.bytes += bytes;
        self.pages.insert(page.page_index, page);
        let Some(last) = self.last_page else {
            return Ok(None);
        };
        if self.pages.len() != last + 1 {
            return Ok(None);
        }
        let mut items = Vec::new();
        let mut ids = HashSet::new();
        for index in 0..=last {
            let page = self.pages.get(&index).context("missing list page")?;
            for item in &page.items {
                ensure!(ids.insert(&item.theme_id), "duplicate listed themeId");
                if let Some(previous) = items.last() {
                    let previous: &InstalledResourcePack = previous;
                    ensure!(
                        previous.theme_id < item.theme_id,
                        "list is not in themeId ASCII order"
                    );
                }
                items.push(item.clone());
            }
        }
        ensure!(
            Some(items.len()) == self.total,
            "completed list does not match total"
        );
        Ok(Some(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(id: &str, index: usize, done: bool, theme: &str) -> Value {
        json!({"msg":"L","replyTo":id,"pageIndex":index,"done":done,"total":2,
            "items":[{"themeId":theme,"name":theme,"metadataStatus":"ok","versionCode":index}]})
    }

    #[test]
    fn collects_all_pages_and_deduplicates_without_accepting_stale_requests() {
        let mut list = ListCollector::new("new".into());
        assert!(list.push(page("old", 0, false, "dark")).unwrap().is_none());
        assert!(list.push(page("new", 1, true, "light")).unwrap().is_none());
        assert!(list.push(page("new", 1, true, "light")).unwrap().is_none());
        let items = list.push(page("new", 0, false, "dark")).unwrap().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].version_code, Some(1));
    }

    #[test]
    fn empty_list_and_legacy_metadata_are_supported() {
        let mut list = ListCollector::new("q".into());
        assert_eq!(
            list.push(
                json!({"msg":"L","replyTo":"q","pageIndex":0,"done":true,"total":0,"items":[]})
            )
            .unwrap(),
            Some(vec![])
        );
        let item: InstalledResourcePack = serde_json::from_value(
            json!({"themeId":"dark","name":"dark","metadataStatus":"unavailable"}),
        )
        .unwrap();
        assert!(item.validate().is_ok());
        assert_eq!(item.version_code, None);
    }

    #[test]
    fn accepts_64_character_theme_ids_and_rejects_65() {
        for (length, valid) in [(64, true), (65, false)] {
            let theme_id = "a".repeat(length);
            let item: InstalledResourcePack = serde_json::from_value(json!({
                "themeId":theme_id,"name":theme_id,"metadataStatus":"unavailable"
            }))
            .unwrap();
            assert_eq!(item.validate().is_ok(), valid);
        }
    }

    #[test]
    fn missing_pages_never_publish_partial_results_and_errors_abort() {
        let mut list = ListCollector::new("q".into());
        assert!(list.push(page("q", 1, true, "light")).unwrap().is_none());
        assert!(
            list.push(json!({"msg":"L","replyTo":"q","errorCode":"list-failed"}))
                .is_err()
        );
    }

    #[test]
    fn rejects_inconsistent_pages_and_bad_versions() {
        let mut list = ListCollector::new("q".into());
        let first = page("q", 0, false, "dark");
        list.push(first.clone()).unwrap();
        let mut duplicate = first;
        duplicate["items"][0]["name"] = json!("different");
        assert!(list.push(duplicate).is_err());
        for code in [
            json!(-1),
            json!(1.5),
            json!(MAX_SAFE_INTEGER + 1),
            json!("1"),
        ] {
            let mut list = ListCollector::new("q".into());
            let mut bad = page("q", 0, false, "dark");
            bad["items"][0]["versionCode"] = code;
            assert!(list.push(bad).is_err());
        }
        let mut list = ListCollector::new("q".into());
        list.push(page("q", 0, false, "dark")).unwrap();
        let mut changed = page("q", 1, true, "light");
        changed["total"] = json!(3);
        assert!(list.push(changed).is_err());
    }

    #[test]
    fn rejects_duplicate_themes_and_final_count_mismatch() {
        let mut list = ListCollector::new("q".into());
        list.push(page("q", 0, false, "dark")).unwrap();
        assert!(list.push(page("q", 1, true, "dark")).is_err());
        let mut list = ListCollector::new("q".into());
        assert!(list.push(page("q", 0, true, "dark")).is_err());
    }
}
