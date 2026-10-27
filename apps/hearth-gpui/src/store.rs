//! `workspaces.json`: `{ id, path, trusted, addedAt }`. New folders start untrusted.
//! Adding a path that is already listed returns that row.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub trusted: bool,
    #[serde(rename = "addedAt")]
    pub added_at: String,
}

pub fn default_path() -> PathBuf {
    if let Ok(path) = std::env::var("HEARTH_WORKSPACE_FILE")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    ns_ui::home_dir().join("Library/Application Support/HearthApp/workspaces.json")
}

pub fn load(path: &Path) -> Result<Vec<Workspace>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("Could not read {}: {error}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))
}

pub fn save(path: &Path, rows: &[Workspace]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot save {}: {error}", path.display()))?;
    }
    let data = serde_json::to_vec_pretty(rows).map_err(|error| error.to_string())?;
    let mut file = data;
    file.push(b'\n');
    fs::write(path, file).map_err(|_| format!("Cannot save {}", path.display()))
}

pub fn add(
    path: &Path,
    rows: &mut Vec<Workspace>,
    input: &str,
) -> Result<(Workspace, bool), String> {
    let folder = normalize(input)?;
    if !Path::new(&folder).is_dir() {
        return Err(format!("Folder does not exist: {folder}"));
    }
    if let Some(row) = rows.iter().find(|row| row.path == folder) {
        return Ok((row.clone(), false));
    }
    let record = Workspace {
        id: new_id(),
        path: folder,
        trusted: false,
        added_at: added_now(),
    };
    rows.push(record.clone());
    if let Err(error) = save(path, rows) {
        rows.pop();
        return Err(error);
    }
    Ok((record, true))
}

pub fn trust(path: &Path, rows: &mut [Workspace], id: &str) -> Result<(), String> {
    let index = rows
        .iter()
        .position(|row| row.id == id)
        .ok_or_else(|| "Workspace not found.".to_string())?;
    let previous = rows[index].trusted;
    rows[index].trusted = true;
    if let Err(error) = save(path, rows) {
        rows[index].trusted = previous;
        return Err(error);
    }
    Ok(())
}

pub fn remove(path: &Path, rows: &mut Vec<Workspace>, id: &str) -> Result<(), String> {
    let Some(index) = rows.iter().position(|row| row.id == id) else {
        return Ok(());
    };
    let removed = rows.remove(index);
    if let Err(error) = save(path, rows) {
        rows.insert(index, removed);
        return Err(error);
    }
    Ok(())
}

pub fn normalize(input: &str) -> Result<String, String> {
    let mut input = input.trim().to_string();
    if input.is_empty() || input.contains('\0') {
        return Err("Path must be an absolute folder.".into());
    }
    if input == "~" || input.starts_with("~/") {
        let home = home();
        input = if input == "~" {
            home.to_string_lossy().into_owned()
        } else {
            format!("{}{}", home.display(), &input[1..])
        };
    }
    if !input.starts_with('/') {
        return Err("Path must be an absolute folder.".into());
    }
    let path = Path::new(&input);
    if path.exists() {
        return Ok(fs::canonicalize(path)
            .unwrap_or(path.to_path_buf())
            .to_string_lossy()
            .into_owned());
    }
    Ok(input)
}

fn added_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix(secs)
}

fn format_unix(mut secs: u64) -> String {
    let days = secs / 86_400;
    secs %= 86_400;
    let hour = secs / 3600;
    let minute = (secs % 3600) / 60;
    let second = secs % 60;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn new_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut file) = fs::File::open("/dev/urandom") {
        let _ = file.read_exact(&mut bytes);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

pub fn home() -> PathBuf {
    ns_ui::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_trust_remove_round_trip() {
        let dir = std::env::temp_dir().join(format!("hearth-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("workspaces.json");
        let mut rows = Vec::new();
        let (row, created) = add(&file, &mut rows, dir.to_str().unwrap()).unwrap();
        assert!(created);
        assert!(!row.trusted);
        let (_, again) = add(&file, &mut rows, dir.to_str().unwrap()).unwrap();
        assert!(!again);
        assert_eq!(rows.len(), 1);
        trust(&file, &mut rows, &row.id).unwrap();
        assert!(load(&file).unwrap()[0].trusted);
        remove(&file, &mut rows, &row.id).unwrap();
        assert!(load(&file).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
