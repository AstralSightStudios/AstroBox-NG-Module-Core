//! Canopus modules on a band (refactor-plan P5).
//!
//! A Canopus module travels as a signed quick-app container named
//! `com.canopus.pkg.<id>`: the phone installs it with the stock quick-app flow
//! (listing, versions, updates and removal come for free) and the band's
//! Canopus Supervisor imports the payload for its own firmware. The module id
//! follows ADR-CAN-003: `^[a-z][a-z0-9_]{0,15}$`; the quick-app `versionCode`
//! is the module's monotonic version code.

use pb::xiaomi::protocol::AppItem;

pub const PACKAGE_PREFIX: &str = "com.canopus.pkg.";

/// ADR-CAN-003 module id syntax.
pub fn is_valid_module_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 16
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// Quick-app package name for a module id, or `None` for an invalid id.
pub fn package_name(module_id: &str) -> Option<String> {
    is_valid_module_id(module_id).then(|| format!("{PACKAGE_PREFIX}{module_id}"))
}

/// Module id of a Canopus package name, or `None` when it is not one.
pub fn module_id(package_name: &str) -> Option<&str> {
    package_name
        .strip_prefix(PACKAGE_PREFIX)
        .filter(|id| is_valid_module_id(id))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanopusModule {
    pub id: String,
    pub package_name: String,
    pub name: String,
    pub version_code: u32,
    pub fingerprint: Vec<u8>,
}

/// Canopus modules among the band's installed quick apps.
pub fn modules_from_apps<'a>(apps: impl IntoIterator<Item = &'a AppItem>) -> Vec<CanopusModule> {
    let mut modules: Vec<CanopusModule> = apps
        .into_iter()
        .filter_map(|app| {
            let id = module_id(&app.package_name)?;
            Some(CanopusModule {
                id: id.to_string(),
                package_name: app.package_name.clone(),
                name: app.app_name.clone(),
                version_code: app.version_code,
                fingerprint: app.fingerprint.clone(),
            })
        })
        .collect();
    modules.sort_by(|a, b| a.id.cmp(&b.id));
    modules
}

/// An available store version that is newer than what the band carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanopusUpdate {
    pub id: String,
    pub installed_version_code: u32,
    pub available_version_code: u32,
}

/// Compares `versionCode`s (the device refuses same or lower versions, so only
/// strictly newer ones are updates).
pub fn updates(installed: &[CanopusModule], available: &[(String, u32)]) -> Vec<CanopusUpdate> {
    available
        .iter()
        .filter_map(|(id, code)| {
            let have = installed.iter().find(|m| &m.id == id)?;
            (*code > have.version_code).then(|| CanopusUpdate {
                id: id.clone(),
                installed_version_code: have.version_code,
                available_version_code: *code,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(package: &str, version: u32) -> AppItem {
        AppItem {
            package_name: package.into(),
            app_name: package.into(),
            version_code: version,
            fingerprint: vec![1, 2, 3],
            ..Default::default()
        }
    }

    #[test]
    fn ids_follow_adr_syntax() {
        assert!(is_valid_module_id("bluetooth_audio"));
        assert!(!is_valid_module_id("Bluetooth"));
        assert!(!is_valid_module_id("org.example.hello"));
        assert!(!is_valid_module_id("a_very_long_module_id"));
        assert_eq!(package_name("lyra_player").as_deref(), Some("com.canopus.pkg.lyra_player"));
        assert_eq!(package_name("../x"), None);
        assert_eq!(module_id("com.canopus.pkg.refetch"), Some("refetch"));
        assert_eq!(module_id("com.canopus.pkg.Bad"), None);
        assert_eq!(module_id("com.other.refetch"), None);
    }

    #[test]
    fn lists_only_canopus_packages_sorted() {
        let apps = vec![
            app("com.xiaomi.thing", 9),
            app("com.canopus.pkg.refetch", 2),
            app("com.canopus.pkg.lyra_player", 7),
        ];
        let modules = modules_from_apps(&apps);
        assert_eq!(modules.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["lyra_player", "refetch"]);
        assert_eq!(modules[0].version_code, 7);
    }

    #[test]
    fn only_strictly_newer_versions_are_updates() {
        let installed = modules_from_apps(&[app("com.canopus.pkg.a", 3), app("com.canopus.pkg.b", 5)]);
        let found = updates(
            &installed,
            &[("a".into(), 4), ("b".into(), 5), ("c".into(), 9)],
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "a");
        assert_eq!(found[0].installed_version_code, 3);
    }
}
