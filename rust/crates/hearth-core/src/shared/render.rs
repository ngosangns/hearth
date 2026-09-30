//! Recipe template rendering. `catalog.json` commands/readiness/connection strings carry
//! `{placeholder}` vars; rendering happens twice — instance vars (`installDir`, `dataDir`,
//! `port`) when the recipe becomes a catalog service definition, and project vars (`projectId`,
//! `projectDb`, `projectUser`, `url`) additionally when provisioning/answering an attach.
use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::catalog::{template_var_spans, unknown_template_var, CommandSpec, ReadinessSpec};

use super::registry::SharedInstance;
use super::{project_bucket_name, project_db_name, project_user_name, SharedError};

/// Instance-level vars — available to every field of a recipe. Extra listeners are `{port2}`,
/// `{port3}`, … in allocation order. A recipe that mentions `{port2}` without a reserved extra
/// port fails `check_vars` rather than spawning with the placeholder intact.
pub fn instance_vars(instance: &SharedInstance, root: &Path) -> HashMap<String, String> {
    let mut vars = HashMap::from([
        (
            "installDir".to_string(),
            instance.install_dir(root).display().to_string(),
        ),
        (
            "dataDir".to_string(),
            instance.data_dir(root).display().to_string(),
        ),
        ("port".to_string(), instance.port.to_string()),
        ("name".to_string(), instance.name.clone()),
        ("version".to_string(), instance.version.clone()),
        ("instanceId".to_string(), instance.id()),
    ]);
    for (index, port) in instance.extra_ports.iter().enumerate() {
        vars.insert(format!("port{}", index + 2), port.to_string());
    }
    vars
}

/// Render a command and reject any `{var}` this context does not define.
pub fn render_command_checked(
    spec: &CommandSpec,
    vars: &HashMap<String, String>,
    what: &str,
) -> Result<CommandSpec, SharedError> {
    for text in command_strings(spec) {
        check_vars(text, vars, what)?;
    }
    Ok(render_command(spec, vars))
}

/// Every template string inside a command — the argv entries, or the one shell string.
pub fn command_strings(spec: &CommandSpec) -> Vec<&str> {
    match spec {
        CommandSpec::Argv { argv } => argv.iter().map(String::as_str).collect(),
        CommandSpec::Shell { shell, .. } => vec![shell.as_str()],
    }
}

/// Adds the per-project vars used by `provision`/`deprovision`/`connection`.
pub fn project_vars(vars: &mut HashMap<String, String>, project_id: &str) {
    vars.insert("projectId".to_string(), project_id.to_string());
    vars.insert("projectDb".to_string(), project_db_name(project_id));
    vars.insert("projectUser".to_string(), project_user_name(project_id));
    vars.insert("projectBucket".to_string(), project_bucket_name(project_id));
}

/// Substitutes every `{name}` var `vars` defines in one pass; unknown vars and `${NAME}` shell
/// expansions are left as written.
pub fn render_str(template: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut last = 0;
    for (start, name) in template_var_spans(template) {
        if let Some(value) = vars.get(name) {
            out.push_str(&template[last..start]);
            out.push_str(value);
            last = start + name.len() + 2;
        }
    }
    out.push_str(&template[last..]);
    out
}

pub fn render_command(spec: &CommandSpec, vars: &HashMap<String, String>) -> CommandSpec {
    match spec {
        CommandSpec::Argv { argv } => CommandSpec::Argv {
            argv: argv.iter().map(|a| render_str(a, vars)).collect(),
        },
        CommandSpec::Shell { shell, exec } => CommandSpec::Shell {
            shell: render_str(shell, vars),
            exec: *exec,
        },
    }
}

/// A `{var}` this context doesn't define means the recipe referenced something it can't have —
/// surface it rather than silently running a command with a literal `{projectDb}` in it. Checked
/// on the template, before rendering; shell `${NAME}` expansions are never template vars.
pub fn check_vars(
    template: &str,
    vars: &HashMap<String, String>,
    what: &str,
) -> Result<(), SharedError> {
    match unknown_template_var(template, |name| vars.contains_key(name)) {
        Some(name) => Err(SharedError(format!(
            "{what} references unknown template var {{{name}}}"
        ))),
        None => Ok(()),
    }
}

pub fn render_readiness(
    spec: &ReadinessSpec,
    vars: &HashMap<String, String>,
) -> Result<ReadinessSpec, SharedError> {
    Ok(match spec {
        ReadinessSpec::Process => ReadinessSpec::Process,
        ReadinessSpec::Tcp { port } => ReadinessSpec::Tcp { port: *port },
        ReadinessSpec::Http { url } => {
            check_vars(url, vars, "readiness.url")?;
            ReadinessSpec::Http {
                url: render_str(url, vars),
            }
        }
        ReadinessSpec::Container => ReadinessSpec::Container,
        ReadinessSpec::Tailnet => ReadinessSpec::Tailnet,
        ReadinessSpec::Command { command, cwd } => ReadinessSpec::Command {
            command: render_command_checked(command, vars, "readiness.command")?,
            cwd: cwd.clone(),
        },
        ReadinessSpec::Exit => ReadinessSpec::Exit,
    })
}

pub fn render_env(
    env: &HashMap<String, String>,
    vars: &HashMap<String, String>,
) -> HashMap<String, String> {
    env.iter()
        .map(|(k, v)| (k.clone(), render_str(v, vars)))
        .collect()
}

/// The attach response's `connection` object: `{ url?, env? }` rendered per-project. `{url}` inside
/// `env` values resolves to the rendered `url`, so recipes write `DATABASE_URL: "{url}"`.
pub fn render_connection(
    connection: &super::remote::SharedConnection,
    vars: &HashMap<String, String>,
) -> Result<Value, SharedError> {
    let mut vars = vars.clone();
    if let Some(url) = &connection.url {
        check_vars(url, &vars, "connection.url")?;
        let url = render_str(url, &vars);
        vars.insert("url".to_string(), url);
    }
    let mut out = serde_json::Map::new();
    if let Some(url) = vars.get("url") {
        out.insert("url".to_string(), json!(url));
    }
    if let Some(env) = &connection.env {
        let mut rendered = serde_json::Map::new();
        for (key, value) in env {
            check_vars(value, &vars, "connection.env")?;
            rendered.insert(key.clone(), json!(render_str(value, &vars)));
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
            extra_ports: vec![],
            install_state: InstallState::Installed,
            install_error: None,
            recipe: SharedRecipe {
                artifacts: HashMap::new(),
                run: CommandSpec::Argv { argv: vec![] },
                stop: None,
                readiness: crate::shared::remote::RecipeReadiness::Spec(ReadinessSpec::Process),
                provision: vec![],
                deprovision: vec![],
                connection: None,
                env: None,
                prepare: None,
                additional_ports: 0,
                ports: Vec::new(),
                extra_port_labels: Vec::new(),
            },
            attachments: BTreeMap::new(),
        }
    }

    #[test]
    fn render_readiness_keeps_exit() {
        let rendered = render_readiness(&ReadinessSpec::Exit, &HashMap::new()).unwrap();
        assert!(matches!(rendered, ReadinessSpec::Exit));
    }

    #[test]
    fn renders_instance_vars_into_commands() {
        let vars = instance_vars(&instance(), Path::new("/shared"));
        let spec = CommandSpec::Argv {
            argv: vec![
                "{installDir}/bin/psql".to_string(),
                "-p".to_string(),
                "{port}".to_string(),
            ],
        };
        let CommandSpec::Argv { argv } = render_command(&spec, &vars) else {
            panic!()
        };
        assert_eq!(
            argv,
            vec!["/shared/installs/postgres/16.4/bin/psql", "-p", "43500"]
        );
    }

    #[test]
    fn renders_connection_with_url_self_reference() {
        let conn = SharedConnection {
            url: Some("postgres://{projectUser}@127.0.0.1:{port}/{projectDb}".to_string()),
            env: Some(HashMap::from([(
                "DATABASE_URL".to_string(),
                "{url}".to_string(),
            )])),
        };
        let mut vars = instance_vars(&instance(), Path::new("/shared"));
        project_vars(&mut vars, "abc123");
        let value = render_connection(&conn, &vars).unwrap();
        let expected = "postgres://u_abc123@127.0.0.1:43500/h_abc123";
        assert_eq!(value["url"], json!(expected));
        assert_eq!(value["env"]["DATABASE_URL"], json!(expected));
    }

    #[test]
    fn checks_every_var_and_leaves_shell_expansions_alone() {
        let vars = instance_vars(&instance(), Path::new("/shared"));
        let spec = CommandSpec::Shell {
            shell: "cd ${HOME} && awk '{print $1}' {dataDir}/x -p {port}".to_string(),
            exec: None,
        };
        let CommandSpec::Shell { shell, .. } = render_command_checked(&spec, &vars, "run").unwrap()
        else {
            panic!()
        };
        assert_eq!(
            shell,
            "cd ${HOME} && awk '{print $1}' /shared/instances/postgres@16.4/x -p 43500"
        );
        // Only the second brace pair is a var — it must still be caught.
        let spec = CommandSpec::Argv {
            argv: vec!["{a b} {port2}".to_string()],
        };
        assert!(render_command_checked(&spec, &vars, "run").is_err());
    }

    #[test]
    fn rejects_unrendered_vars() {
        let vars = HashMap::new();
        let conn = SharedConnection {
            url: Some("postgres://{projectUser}".to_string()),
            env: None,
        };
        assert!(render_connection(&conn, &vars).is_err());
    }
}
