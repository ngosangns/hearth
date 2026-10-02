//! The workspace list for `hearthd tui`.
//!
//! The file is `~/Library/Application Support/HearthApp/workspaces.json`. Swift encodes each row
//! as `{ id, path, trusted, addedAt }` with an ISO-8601 timestamp and no fractional seconds.
//! A file that exists but does not decode is moved aside: the next save must not replace the
//! user's list with an empty one.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config_file::load_catalog;
use crate::supervisor::types::format_iso8601_millis;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub id: String,
    pub path: String,
    pub trusted: bool,
    pub added_at: String,
}

pub struct AddedWorkspace {
    pub record: WorkspaceRecord,
    pub created: bool,
}

pub struct WorkspaceStore {
    path: PathBuf,
    rows: Vec<WorkspaceRecord>,
    pub load_error: Option<String>,
}

impl WorkspaceStore {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
        }
        let (rows, load_error) = load_rows(&path);
        Ok(Self {
            path,
            rows,
            load_error,
        })
    }

    pub fn list(&self) -> &[WorkspaceRecord] {
        &self.rows
    }

    pub fn get(&self, id: &str) -> Option<&WorkspaceRecord> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// Adds `path` when it is a new directory. The same folder twice returns the existing row and
    /// does not change `trusted`. New rows start untrusted: a `hearth.yaml` names commands, and
    /// nothing is spawned until the operator confirms.
    pub fn add(&mut self, path: &str) -> Result<AddedWorkspace, String> {
        let path = normalize_path(path)?;
        if !Path::new(&path).is_dir() {
            return Err(format!("folder does not exist: {path}"));
        }
        if let Some(existing) = self.rows.iter().find(|row| row.path == path) {
            return Ok(AddedWorkspace {
                record: existing.clone(),
                created: false,
            });
        }
        let record = WorkspaceRecord {
            id: uuid::Uuid::new_v4().to_string().to_ascii_uppercase(),
            path,
            trusted: false,
            added_at: added_now(),
        };
        self.rows.push(record.clone());
        if let Err(error) = self.save() {
            self.rows.pop();
            return Err(error);
        }
        Ok(AddedWorkspace {
            record,
            created: true,
        })
    }

    /// When `root` has a catalog, make sure it is in the list (still untrusted if it is new) and
    /// return its id. A directory with no `hearth.yaml` is left alone.
    pub fn adopt_project(&mut self, root: &Path) -> Option<String> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if load_catalog(&root).is_err() {
            return None;
        }
        let path = root.to_str()?;
        match self.add(path) {
            Ok(added) => Some(added.record.id),
            Err(error) => {
                eprintln!("could not remember {}: {error}", root.display());
                None
            }
        }
    }

    pub fn trust(&mut self, id: &str) -> Result<WorkspaceRecord, String> {
        let Some(index) = self.rows.iter().position(|row| row.id == id) else {
            return Err("workspace not found".to_string());
        };
        let previous = self.rows[index].trusted;
        self.rows[index].trusted = true;
        let record = self.rows[index].clone();
        if let Err(error) = self.save() {
            self.rows[index].trusted = previous;
            return Err(error);
        }
        Ok(record)
    }

    /// Re-reads the file. A read or parse failure leaves the in-memory list alone and does not
    /// quarantine the file — a refresh tick must not move the user's list aside.
    pub fn reload(&mut self) -> Result<(), String> {
        match fs::read(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.rows.clear();
                self.load_error = None;
                Ok(())
            }
            Err(error) => Err(format!("could not read {}: {error}", self.path.display())),
            Ok(data) if data.is_empty() => {
                self.rows.clear();
                self.load_error = None;
                Ok(())
            }
            Ok(data) => match serde_json::from_slice::<Vec<WorkspaceRecord>>(&data) {
                Ok(rows) => {
                    self.rows = rows;
                    self.load_error = None;
                    Ok(())
                }
                Err(error) => Err(format!(
                    "{} could not be read: {error}",
                    self.path.display()
                )),
            },
        }
    }

    pub fn remove(&mut self, id: &str) -> Result<bool, String> {
        let Some(index) = self.rows.iter().position(|row| row.id == id) else {
            return Ok(false);
        };
        let removed = self.rows.remove(index);
        if let Err(error) = self.save() {
            self.rows.insert(index, removed);
            return Err(error);
        }
        Ok(true)
    }

    fn save(&self) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(&self.rows).map_err(|error| error.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, json)
            .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .map_err(|error| format!("cannot save {}: {error}", self.path.display()))
    }
}

fn load_rows(path: &Path) -> (Vec<WorkspaceRecord>, Option<String>) {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(error) => {
            return (
                Vec::new(),
                Some(format!("could not read {}: {error}", path.display())),
            )
        }
    };
    if data.is_empty() {
        return (Vec::new(), None);
    }
    match serde_json::from_slice::<Vec<WorkspaceRecord>>(&data) {
        Ok(rows) => (rows, None),
        Err(error) => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspaces.json");
            let aside = path.with_file_name(format!("{name}.corrupt-{stamp}"));
            let message = if fs::rename(path, &aside).is_ok() {
                format!("{name} could not be read and was moved to {}. Starting with an empty workspace list.", aside.display())
            } else {
                format!("{name} could not be read: {error}")
            };
            (Vec::new(), Some(message))
        }
    }
}

/// Absolute path, `~` expanded, symlinks resolved when the folder exists.
pub fn normalize_path(input: &str) -> Result<String, String> {
    if input.is_empty() || input.contains('\0') {
        return Err("path must be an absolute folder".to_string());
    }
    let expanded = if input == "~" {
        home_dir()?
    } else if let Some(rest) = input.strip_prefix("~/") {
        home_dir()?.join(rest)
    } else {
        PathBuf::from(input)
    };
    if expanded.is_relative() {
        return Err("path must be absolute".to_string());
    }
    let resolved = if expanded.exists() {
        expanded.canonicalize().map_err(|error| error.to_string())?
    } else {
        expanded
    };
    resolved
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "path is not valid UTF-8".to_string())
}

pub fn display_path(path: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.to_string();
    };
    let home = PathBuf::from(&home);
    let home = home.canonicalize().unwrap_or(home);
    let Some(home) = home.to_str() else {
        return path.to_string();
    };
    if path == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    path.strip_prefix(&prefix)
        .map(|rest| format!("~/{rest}"))
        .unwrap_or_else(|| path.to_string())
}

pub fn folder_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "HOME is not set".to_string())
}

/// Swift's `.iso8601` strategy rejects fractional seconds, so the shared file omits them.
fn added_now() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let full = format_iso8601_millis(millis);
    match full.find('.') {
        Some(index) => format!("{}Z", &full[..index]),
        None => full,
    }
}

pub fn default_workspace_file() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/HearthApp/workspaces.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_keeps_the_list_when_the_file_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        let mut store = WorkspaceStore::open(path.clone()).unwrap();
        let added = store.add(dir.path().to_str().unwrap()).unwrap();
        assert!(added.created);
        fs::write(&path, b"{not-json").unwrap();
        let error = store.reload().unwrap_err();
        assert!(error.contains("could not be read"), "{error}");
        assert_eq!(store.list().len(), 1);
        assert!(path.exists(), "a refresh must not quarantine the file");
    }
}
