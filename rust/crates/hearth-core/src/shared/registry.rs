//! `registry.json` — smp's instance table. One writer (the smp daemon); CLI processes only ever
//! mutate it through the daemon's HTTP API. Follows the same discipline as `state.json`: atomic
//! writes, and a corrupt file is quarantined rather than trusted.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::file_io::FileIo;

use super::remote::SharedRecipe;
use super::SharedError;

const REGISTRY_FILE_NAME: &str = "registry.json";
const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallState {
    Pending,
    Installing,
    Installed,
    Failed,
}

impl InstallState {
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Installing => "installing",
            Self::Installed => "installed",
            Self::Failed => "failed",
        }
    }
}

/// One attached project. `connection` is the recipe's `connection` block rendered for this
/// project's template vars, cached at provision time so re-attaches are cheap and stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedAttachment {
    pub project_root: String,
    pub provisioned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub attached_at: String,
}

/// A registered `name@version` singleton. `recipe` is the snapshot taken at registration —
/// installs never consult the remote registry again for this instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedInstance {
    pub name: String,
    pub version: String,
    /// Resolved listen port — derived deterministically, collision-shifted at registration.
    /// Extra listeners (controller, console, …) live in `extra_ports`, contiguous after this one.
    pub port: u16,
    /// Additional ports reserved with `port` as one contiguous block. Empty on registries written
    /// before multi-port recipes existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_ports: Vec<u16>,
    pub install_state: InstallState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_error: Option<String>,
    pub recipe: SharedRecipe,
    #[serde(default)]
    pub attachments: BTreeMap<String, SharedAttachment>,
}

impl SharedInstance {
    pub fn id(&self) -> String {
        super::instance_id(&self.name, &self.version)
    }
    pub fn install_dir(&self, root: &Path) -> PathBuf {
        root.join("installs").join(&self.name).join(&self.version)
    }
    pub fn data_dir(&self, root: &Path) -> PathBuf {
        root.join("instances").join(self.id())
    }
    /// Primary port, then every extra port. The allocator treats the whole set as taken.
    pub fn all_ports(&self) -> Vec<u16> {
        let mut ports = Vec::with_capacity(1 + self.extra_ports.len());
        ports.push(self.port);
        ports.extend(self.extra_ports.iter().copied());
        ports
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SharedRegistryData {
    #[serde(default)]
    pub instances: BTreeMap<String, SharedInstance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    #[serde(flatten)]
    data: SharedRegistryData,
}

pub struct SharedRegistry {
    io: Arc<dyn FileIo>,
    path: PathBuf,
    data: Mutex<SharedRegistryData>,
}

impl SharedRegistry {
    /// Loads `registry.json`; a present-but-corrupt file is renamed aside and the registry starts
    /// empty — same quarantine discipline as `state.json`, because a panic here would kill the
    /// daemon at bootstrap.
    pub fn load(io: Arc<dyn FileIo>, root: &Path) -> Result<Self, SharedError> {
        let path = root.join(REGISTRY_FILE_NAME);
        let data = match io
            .read_file(&path)
            .map_err(|e| SharedError(e.to_string()))?
        {
            None => SharedRegistryData::default(),
            Some(text) => match serde_json::from_str::<RegistryFile>(&text) {
                Ok(file) if file.version == REGISTRY_VERSION => file.data,
                _ => {
                    let _ = io.quarantine(&path, "corrupt");
                    SharedRegistryData::default()
                }
            },
        };
        Ok(Self {
            io,
            path,
            data: Mutex::new(data),
        })
    }

    pub fn list(&self) -> Vec<SharedInstance> {
        self.data
            .lock()
            .unwrap()
            .instances
            .values()
            .cloned()
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<SharedInstance> {
        self.data.lock().unwrap().instances.get(id).cloned()
    }

    /// Mutate + persist atomically. The closure runs under the registry mutex; keep it sync and
    /// small.
    pub fn update<R>(
        &self,
        f: impl FnOnce(&mut SharedRegistryData) -> R,
    ) -> Result<R, SharedError> {
        let result = {
            let mut data = self.data.lock().unwrap();
            let result = f(&mut data);
            let file = RegistryFile {
                version: REGISTRY_VERSION,
                data: data.clone(),
            };
            (result, serde_json::to_string_pretty(&file).unwrap())
        };
        self.io
            .write_file(&self.path, &result.1)
            .map_err(|e| SharedError(e.to_string()))?;
        Ok(result.0)
    }

    pub fn remove(&self, id: &str) -> Result<Option<SharedInstance>, SharedError> {
        self.update(|data| data.instances.remove(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CommandSpec, ReadinessSpec};
    use crate::file_io::create_file_io;
    use crate::shared::remote::{SharedArtifact, SharedConnection};
    use std::collections::HashMap;

    fn recipe() -> SharedRecipe {
        SharedRecipe {
            artifacts: HashMap::from([(
                "darwin-arm64".to_string(),
                SharedArtifact {
                    url: Some("file:///tmp/x.tgz".to_string()),
                    sha256: "abc".to_string(),
                    script: None,
                    script_args: vec![],
                },
            )]),
            run: CommandSpec::Argv {
                argv: vec!["run".to_string()],
            },
            stop: None,
            readiness: ReadinessSpec::Process,
            provision: vec![],
            deprovision: vec![],
            connection: Some(SharedConnection {
                url: Some("x://{port}".to_string()),
                env: None,
            }),
            env: None,
            prepare: None,
            additional_ports: 0,
            ports: Vec::new(),
            extra_port_labels: Vec::new(),
        }
    }

    fn instance() -> SharedInstance {
        SharedInstance {
            name: "redis".to_string(),
            version: "7.2".to_string(),
            port: 43123,
            extra_ports: vec![],
            install_state: InstallState::Installed,
            install_error: None,
            recipe: recipe(),
            attachments: BTreeMap::new(),
        }
    }

    #[test]
    fn roundtrips_an_instance() {
        let dir = tempfile::tempdir().unwrap();
        let io: Arc<dyn FileIo> = Arc::from(create_file_io(false));
        let registry = SharedRegistry::load(io.clone(), dir.path()).unwrap();
        registry
            .update(|d| {
                d.instances.insert("redis@7.2".to_string(), instance());
            })
            .unwrap();

        let reloaded = SharedRegistry::load(io, dir.path()).unwrap();
        let got = reloaded.get("redis@7.2").unwrap();
        assert_eq!(got.port, 43123);
        assert_eq!(got.install_state, InstallState::Installed);
        assert_eq!(got.id(), "redis@7.2");
    }

    #[test]
    fn loads_a_registry_written_before_extra_ports() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = r#"{"version":1,"instances":{"redis@7.2":{"name":"redis","version":"7.2","port":43123,"installState":"installed","recipe":{"artifacts":{},"run":{"argv":["redis-server"]},"readiness":{"kind":"process"}},"attachments":{}}}}"#;
        std::fs::write(dir.path().join(REGISTRY_FILE_NAME), legacy).unwrap();
        let registry =
            SharedRegistry::load(Arc::<dyn FileIo>::from(create_file_io(false)), dir.path())
                .unwrap();
        let got = registry.get("redis@7.2").unwrap();
        assert!(got.extra_ports.is_empty());
        assert!(got.all_ports() == vec![43123]);
        assert!(got.recipe.prepare.is_none());
        assert_eq!(got.recipe.additional_ports, 0);
        assert!(got.recipe.extra_port_labels.is_empty());
    }

    #[test]
    fn quarantines_a_corrupt_registry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(REGISTRY_FILE_NAME), "not json {{{").unwrap();
        let registry =
            SharedRegistry::load(Arc::<dyn FileIo>::from(create_file_io(false)), dir.path())
                .unwrap();
        assert!(registry.list().is_empty());
        assert!(!dir.path().join(REGISTRY_FILE_NAME).exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("corrupt")));
    }
}
