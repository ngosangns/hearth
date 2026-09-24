//! Recipe template rendering. `catalog.json` commands/readiness/connection strings carry
//! `{placeholder}` vars; rendering happens twice — instance vars (`installDir`, `dataDir`,
//! `port`) when the recipe becomes a catalog service definition, and project vars (`projectId`,
//! `projectDb`, `projectUser`, `url`) additionally when provisioning/answering an attach.
use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::catalog::{CommandSpec, ReadinessSpec};

use super::registry::SharedInstance;
use super::{project_db_name, project_user_name, SharedError};

/// Instance-level vars — available to every field of a recipe.
pub fn instance_vars(instance: &SharedInstance, root: &Path) -> HashMap<String, String> {
    HashMap::from([
        ("installDir".to_string(), instance.install_dir(root).display().to_string()),
        ("dataDir".to_string(), instance.data_dir(root).display().to_string()),
        ("port".to_string(), instance.port.to_string()),
        ("name".to_string(), instance.name.clone()),
        ("version".to_string(), instance.version.clone()),
        ("instanceId".to_string(), instance.id()),
    ])
}

/// Adds the per-project vars used by `provision`/`deprovision`/`connection`.
pub fn project_vars(vars: &mut HashMap<String, String>, project_id: &str) {
    vars.insert("projectId".to_string(), project_id.to_string());
    vars.insert("projectDb".to_string(), project_db_name(project_id));
    vars.insert("projectUser".to_string(), project_user_name(project_id));
}

pub fn render_str(template: &str, vars: &HashMap<String, String>) -> String {
    let mut out = template.to_string();
    for (key, value) in vars {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

pub fn render_command(spec: &CommandSpec, vars: &HashMap<String, String>) -> CommandSpec {
    match spec {
        CommandSpec::Argv { argv } => CommandSpec::Argv { argv: argv.iter().map(|a| render_str(a, vars)).collect() },
        CommandSpec::Shell { shell, exec } => CommandSpec::Shell { shell: render_str(shell, vars), exec: *exec },
    }
}

/// Leftover `{var}` after rendering means the recipe referenced a var this context doesn't define
/// — surface it rather than silently running a command with a literal `{projectDb}` in it.
pub fn check_unrendered(rendered: &str, what: &str) -> Result<(), SharedError> {
    if let Some(rest) = rendered.split('{').nth(1) {
        if let Some(name) = rest.split('}').next() {
            if name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                return Err(SharedError(format!("{what} references unknown template var {{{name}}}")));
            }
        }
    }
    Ok(())
}

pub fn render_readiness(spec: &ReadinessSpec, vars: &HashMap<String, String>) -> Result<ReadinessSpec, SharedError> {
    Ok(match spec {
        ReadinessSpec::Process => ReadinessSpec::Process,
        ReadinessSpec::Tcp { port } => ReadinessSpec::Tcp { port: *port },
        ReadinessSpec::Http { url } => {
            let url = render_str(url, vars);
            check_unrendered(&url, "readiness.url")?;
            ReadinessSpec::Http { url }
        }
        ReadinessSpec::Container => ReadinessSpec::Container,
        ReadinessSpec::Tailnet => ReadinessSpec::Tailnet,
        ReadinessSpec::Command { command, cwd } => ReadinessSpec::Command { command: render_command(command, vars), cwd: cwd.clone() },
    })
}

pub fn render_env(env: &HashMap<String, String>, vars: &HashMap<String, String>) -> HashMap<String, String> {
    env.iter().map(|(k, v)| (k.clone(), render_str(v, vars))).collect()
}

/// The attach response's `connection` object: `{ url?, env? }` rendered per-project. `{url}` inside
/// `env` values resolves to the rendered `url`, so recipes write `DATABASE_URL: "{url}"`.
pub fn render_connection(connection: &super::remote::SharedConnection, vars: &HashMap<String, String>) -> Result<Value, SharedError> {
    let mut vars = vars.clone();
    if let Some(url) = &connection.url {
        let url = render_str(url, &vars);
        check_unrendered(&url, "connection.url")?;
        vars.insert("url".to_string(), url.clone());
    }
    let mut out = serde_json::Map::new();
    if let Some(url) = vars.get("url") {
        out.insert("url".to_string(), json!(url));
    }
    if let Some(env) = &connection.env {
        let mut rendered = serde_json::Map::new();
        for (key, value) in env {
            let value = render_str(value, &vars);
            check_unrendered(&value, "connection.env")?;
            rendered.insert(key.clone(), json!(value));
        }
        out.insert("env".to_string(), Value::Object(rendered));
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::registry::InstallState;
    use crate::shared::remote::{SharedConnection, SharedRecipe};
    use std::collections::BTreeMap;

    fn instance() -> SharedInstance {
        SharedInstance {
            name: "postgres".to_string(),
            version: "16.4".to_string(),
            port: 43500,
            install_state: InstallState::Installed,
            install_error: None,
            recipe: SharedRecipe {
                artifacts: HashMap::new(),
                run: CommandSpec::Argv { argv: vec![] },
                stop: None,
                readiness: ReadinessSpec::Process,
                provision: vec![],
                deprovision: vec![],
                connection: None,
                env: None,
            },
            attachments: BTreeMap::new(),
        }
    }

    #[test]
    fn renders_instance_vars_into_commands() {
        let vars = instance_vars(&instance(), Path::new("/shared"));
        let spec = CommandSpec::Argv { argv: vec!["{installDir}/bin/psql".to_string(), "-p".to_string(), "{port}".to_string()] };
        let CommandSpec::Argv { argv } = render_command(&spec, &vars) else { panic!() };
        assert_eq!(argv, vec!["/shared/installs/postgres/16.4/bin/psql", "-p", "43500"]);
    }

    #[test]
    fn renders_connection_with_url_self_reference() {
        let conn = SharedConnection {
            url: Some("postgres://{projectUser}@127.0.0.1:{port}/{projectDb}".to_string()),
            env: Some(HashMap::from([("DATABASE_URL".to_string(), "{url}".to_string())])),
        };
        let mut vars = instance_vars(&instance(), Path::new("/shared"));
        project_vars(&mut vars, "abc123");
        let value = render_connection(&conn, &vars).unwrap();
        let expected = "postgres://u_abc123@127.0.0.1:43500/h_abc123";
        assert_eq!(value["url"], json!(expected));
        assert_eq!(value["env"]["DATABASE_URL"], json!(expected));
    }

    #[test]
    fn rejects_unrendered_vars() {
        let vars = HashMap::new();
        let conn = SharedConnection { url: Some("postgres://{projectUser}".to_string()), env: None };
        assert!(render_connection(&conn, &vars).is_err());
    }
}
