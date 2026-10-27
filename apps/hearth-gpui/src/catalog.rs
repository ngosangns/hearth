//! The bits of `hearth.yaml` the window needs: service groups, disabled
//! services, shared names, and the service keys. Live status still wins when
//! the daemon answers. These ids are the list shown while it does not.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Group name to members, in first-declaration order. A service joins the
    /// group its own optional `group:` names; there is no `groups:` block and
    /// a group cannot contain another group.
    pub groups: Vec<(String, Vec<String>)>,
    pub disabled: Vec<String>,
    pub shared: Vec<String>,
    pub services: Vec<String>,
}

pub fn parse_catalog(text: &str) -> Catalog {
    let mut catalog = Catalog::default();
    let mut group_of = std::collections::HashMap::<String, String>::new();
    let mut section = "";
    let mut entry = String::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.chars().take_while(|ch| *ch == ' ').count();
        let trimmed = line.trim();
        if indent == 0 && trimmed.ends_with(':') {
            section = match trimmed.trim_end_matches(':') {
                "services" => "services",
                "shared" => "shared",
                _ => "",
            };
            entry.clear();
            continue;
        }
        match section {
            "services" if indent == 2 && trimmed.ends_with(':') => {
                entry = bare_name(trimmed.trim_end_matches(':'));
                if !entry.is_empty() && !catalog.services.iter().any(|item| item == &entry) {
                    catalog.services.push(entry.clone());
                }
            }
            "services" if indent >= 4 && !entry.is_empty() => {
                if trimmed == "disabled: true" {
                    if !catalog.disabled.iter().any(|item| item == &entry) {
                        catalog.disabled.push(entry.clone());
                    }
                } else if let Some(group) = group_value(trimmed) {
                    group_of.insert(entry.clone(), group);
                }
            }
            "shared" if indent == 2 => {
                let (name, rest) = trimmed.split_once(':').unwrap_or((trimmed, ""));
                entry = bare_name(name);
                if !entry.is_empty() && !catalog.shared.iter().any(|item| item == &entry) {
                    catalog.shared.push(entry.clone());
                }
                if let Some(group) = group_value(rest.trim()) {
                    group_of.insert(entry.clone(), group);
                }
            }
            "shared" if indent >= 4 && !entry.is_empty() => {
                if let Some(group) = group_value(trimmed) {
                    group_of.insert(entry.clone(), group);
                }
            }
            _ => {}
        }
    }
    // The daemon derives groups from its service list: project services in
    // document order, then shared entries. Mirror that so membership order
    // matches the served catalog. `all` is the CLI's own target, not a group.
    let mut index_of = std::collections::HashMap::<String, usize>::new();
    for id in catalog.services.iter().chain(catalog.shared.iter()) {
        let Some(group) = group_of.get(id.as_str()) else {
            continue;
        };
        match index_of.get(group) {
            Some(&index) => catalog.groups[index].1.push(id.clone()),
            None => {
                index_of.insert(group.clone(), catalog.groups.len());
                catalog.groups.push((group.clone(), vec![id.clone()]));
            }
        }
    }
    catalog
}

fn bare_name(raw: &str) -> String {
    raw.trim().trim_matches('"').trim_matches('\'').to_string()
}

/// The value of a `group:` key on this line, either as the block key or inside
/// a one-line flow map (`{ version: "8", group: infra }`).
fn group_value(line: &str) -> Option<String> {
    let rest = match line.strip_prefix("group:") {
        Some(rest) => rest,
        None => line
            .strip_prefix('{')?
            .split_once(", group:")
            .map(|(_, rest)| rest)?,
    };
    let raw = rest.split([',', '}']).next().unwrap_or("");
    let raw = raw.split(" #").next().unwrap_or("");
    let name = bare_name(raw);
    // The daemon rejects an empty, spaced, or reserved name, so a config that
    // carries one never loads. Do not draw a group it would not serve.
    if name.is_empty() || name == "all" || name.contains(char::is_whitespace) {
        return None;
    }
    Some(name)
}

/// Ids to draw when `status --json` is not a catalog. Shared names come first,
/// then keys under `services:`. A name that is both is kept once.
pub fn offline_ids(catalog: &Catalog) -> Vec<String> {
    let mut ids = Vec::new();
    for name in catalog.shared.iter().chain(catalog.services.iter()) {
        if !ids.iter().any(|item| item == name) {
            ids.push(name.clone());
        }
    }
    ids
}

/// Members of a group, or the name itself when it is a service id.
pub fn expand(name: &str, groups: &[(String, Vec<String>)]) -> Vec<String> {
    match groups.iter().find(|(group, _)| group == name) {
        Some((_, members)) => members.clone(),
        None => vec![name.to_string()],
    }
}

/// One block of the service list. `actions` is set for a real catalog group.
/// `Other` and `Services` are synthetic and have no group start/stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceGroup {
    pub title: String,
    pub actions: bool,
    pub indexes: Vec<usize>,
}

/// Groups that still have an enabled member. An empty group stays.
pub fn visible_groups(
    groups: &[(String, Vec<String>)],
    disabled: &[String],
) -> Vec<(String, Vec<String>)> {
    groups
        .iter()
        .filter(|(_, members)| {
            members.is_empty() || !members.iter().all(|member| disabled.contains(member))
        })
        .cloned()
        .collect()
}

/// Place each service under the group that names it. Services with no group
/// trail under `Other`, or `Services` when nothing declares a group.
/// `hidden` ids are omitted from every section, including `Other`.
pub fn service_groups_hiding(
    ids: &[String],
    groups: &[(String, Vec<String>)],
    hidden: &[String],
) -> Vec<ServiceGroup> {
    let mut index_of = std::collections::HashMap::<&str, usize>::new();
    for (index, id) in ids.iter().enumerate() {
        index_of.entry(id.as_str()).or_insert(index);
    }
    let mut assigned = vec![false; ids.len()];
    let mut out = Vec::new();
    for (name, members) in groups {
        let mut indexes = Vec::new();
        for member in members {
            let Some(&index) = index_of.get(member.as_str()) else {
                continue;
            };
            if assigned[index] {
                continue;
            }
            assigned[index] = true;
            if hidden.iter().any(|item| item == &ids[index]) {
                continue;
            }
            indexes.push(index);
        }
        if !indexes.is_empty() {
            out.push(ServiceGroup {
                title: name.clone(),
                actions: true,
                indexes,
            });
        }
    }
    let rest: Vec<usize> = assigned
        .iter()
        .enumerate()
        .filter(|(index, taken)| !**taken && !hidden.iter().any(|item| item == &ids[*index]))
        .map(|(index, _)| index)
        .collect();
    if !rest.is_empty() {
        out.push(ServiceGroup {
            title: if groups.is_empty() {
                "Services".into()
            } else {
                "Other".into()
            },
            actions: false,
            indexes: rest,
        });
    }
    out
}

/// The first observation only records the stamp. A later, different stamp reloads the daemon catalog.
pub fn should_reload_catalog(previous: Option<u64>, next: Option<u64>) -> bool {
    matches!((previous, next), (Some(before), Some(after)) if before != after)
}

/// `hearth.yaml` mtimes per workspace root. One window-wide stamp would
/// compare one folder's mtime with another's on every switch.
#[derive(Default)]
pub struct CatalogStamps(std::collections::HashMap<String, u64>);

impl CatalogStamps {
    pub fn previous(&self, root: &str) -> Option<u64> {
        self.0.get(root).copied()
    }

    pub fn record(&mut self, root: &str, stamp: Option<u64>) {
        match stamp {
            Some(stamp) => {
                self.0.insert(root.to_owned(), stamp);
            }
            None => {
                self.0.remove(root);
            }
        }
    }
}

pub fn touches_shared(ids: &[String], shared: &[String]) -> bool {
    ids.iter().any(|id| shared.iter().any(|item| item == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_groups_disabled_and_shared() {
        let catalog = parse_catalog(
            "shared:\n  mongodb: { version: \"8\", group: infra }\n  redis: \"8\"\nservices:\n  nats:\n    group: infra\n    disabled: true\n  api:\n    group: backend\n    run: {}\n",
        );
        assert_eq!(catalog.shared, vec!["mongodb", "redis"]);
        assert_eq!(catalog.services, vec!["nats", "api"]);
        assert_eq!(catalog.disabled, vec!["nats"]);
        // Services first, then shared entries, as the daemon builds them.
        assert_eq!(
            catalog.groups,
            vec![
                ("infra".into(), vec!["nats".into(), "mongodb".into()]),
                ("backend".into(), vec!["api".into()]),
            ]
        );
        let commented = parse_catalog(
            "shared:\n  mongodb:\n    version: \"8\"\n    group: infra\n  # not a service: nope\nservices:\n  api:\n    # group: nope\n    run: {}\n",
        );
        assert_eq!(commented.shared, vec!["mongodb"]);
        assert_eq!(commented.services, vec!["api"]);
        assert_eq!(
            commented.groups,
            vec![("infra".into(), vec!["mongodb".into()])]
        );
        assert!(commented.disabled.is_empty());
        assert!(touches_shared(
            &expand("infra", &commented.groups),
            &commented.shared
        ));
        assert_eq!(expand("api", &catalog.groups), vec!["api"]);
        assert!(!should_reload_catalog(None, Some(10)));
        assert!(!should_reload_catalog(Some(10), Some(10)));
        assert!(should_reload_catalog(Some(10), Some(11)));
    }

    #[test]
    fn the_removed_groups_block_is_ignored_and_all_is_reserved() {
        let catalog = parse_catalog(
            "groups:\n  infra: [mongodb, redis]\nshared:\n  mongodb: \"8\"\nservices:\n  api:\n    group: all\n    run: {}\n  web:\n    group: front end\n    run: {}\n",
        );
        assert_eq!(catalog.shared, vec!["mongodb"]);
        assert_eq!(catalog.services, vec!["api", "web"]);
        assert!(catalog.groups.is_empty());
    }

    #[test]
    fn catalog_stamps_are_tracked_per_root() {
        let mut stamps = CatalogStamps::default();
        // First visit of each root only records.
        assert!(!should_reload_catalog(
            stamps.previous("/viclass"),
            Some(10)
        ));
        stamps.record("/viclass", Some(10));
        assert!(!should_reload_catalog(stamps.previous("/infra"), Some(99)));
        stamps.record("/infra", Some(99));
        // Switching back compares against the same root's own stamp.
        assert!(!should_reload_catalog(
            stamps.previous("/viclass"),
            Some(10)
        ));
        assert!(!should_reload_catalog(stamps.previous("/infra"), Some(99)));
        // A real edit to one root reloads only that root.
        assert!(should_reload_catalog(stamps.previous("/infra"), Some(100)));
        assert!(!should_reload_catalog(
            stamps.previous("/viclass"),
            Some(10)
        ));
        // A vanished file forgets the stamp, so its return only records.
        stamps.record("/infra", None);
        assert!(!should_reload_catalog(stamps.previous("/infra"), Some(101)));
    }

    #[test]
    fn offline_ids_keep_shared_names_and_service_keys() {
        let catalog = parse_catalog(
            "shared:\n  mongodb: { version: \"8.0.32\", group: infra }\n  redis: \"8\"\nservices:\n  api:\n    group: infra\n    run: {}\n  nats:\n    group: core\n    disabled: true\n  \"mongodb\":\n    run: {}\n",
        );
        let ids = offline_ids(&catalog);
        assert_eq!(ids, vec!["mongodb", "redis", "api", "nats"]);
        let grouped = service_groups_hiding(&ids, &catalog.groups, &[]);
        assert_eq!(names(&grouped[0], &ids), ("infra", vec!["api", "mongodb"]));
        assert_eq!(names(&grouped[1], &ids), ("core", vec!["nats"]));
        assert_eq!(names(&grouped[2], &ids), ("Other", vec!["redis"]));
    }

    #[test]
    fn hiding_disabled_drops_a_group_whose_services_are_all_disabled() {
        let groups = vec![
            ("infra".into(), vec!["mongodb".into(), "redis".into()]),
            ("core".into(), vec!["api".into()]),
            ("tools".into(), vec!["shorturl".into()]),
            ("empty".into(), Vec::new()),
        ];
        let ids = ["mongodb", "redis", "api", "shorturl", "fe"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let disabled = vec!["mongodb".into(), "redis".into(), "shorturl".into()];
        let groups = visible_groups(&groups, &disabled);
        let titles: Vec<_> = groups.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(titles, vec!["core", "empty"]);
        assert_eq!(groups[0].1, vec!["api"]);
        let grouped = service_groups_hiding(&ids, &groups, &disabled);
        assert_eq!(names(&grouped[0], &ids), ("core", vec!["api"]));
        assert_eq!(names(&grouped[1], &ids), ("Other", vec!["fe"]));
        assert!(grouped.iter().all(|group| group.title != "infra"));
        assert!(grouped.iter().all(|group| group.title != "tools"));
    }

    #[test]
    fn groups_services_under_the_group_that_names_them() {
        let groups = vec![
            ("infra".into(), vec!["mongodb".into(), "redis".into()]),
            ("core".into(), vec!["api".into()]),
        ];
        let ids = ["mongodb", "redis", "api", "syncer", "fe"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let grouped = service_groups_hiding(&ids, &groups, &[]);
        assert_eq!(
            names(&grouped[0], &ids),
            ("infra", vec!["mongodb", "redis"])
        );
        assert!(grouped[0].actions);
        assert_eq!(names(&grouped[1], &ids), ("core", vec!["api"]));
        assert_eq!(names(&grouped[2], &ids), ("Other", vec!["syncer", "fe"]));
        assert!(!grouped[2].actions);
        let ungrouped = service_groups_hiding(&ids, &[], &[]);
        assert_eq!(ungrouped[0].title, "Services");
        assert!(!ungrouped[0].actions);
    }

    fn names<'a>(group: &'a ServiceGroup, ids: &'a [String]) -> (&'a str, Vec<&'a str>) {
        (
            group.title.as_str(),
            group
                .indexes
                .iter()
                .map(|index| ids[*index].as_str())
                .collect(),
        )
    }
}
