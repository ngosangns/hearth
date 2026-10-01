use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hearth_web::{parse_web_args, serve, ServeOptions, SpawnHook, WebArgs, DEFAULT_PORT};

fn noop_hook(flag: &Arc<AtomicBool>) -> SpawnHook {
    let flag = flag.clone();
    Arc::new(move |_path: &Path| {
        flag.store(true, Ordering::SeqCst);
    })
}

struct Server {
    running: hearth_web::Running,
    spawned: Arc<AtomicBool>,
}

impl Server {
    async fn start(workspace_file: &Path) -> Self {
        let spawned = Arc::new(AtomicBool::new(false));
        let running = serve(ServeOptions {
            port: 0,
            workspace_file: workspace_file.to_path_buf(),
            adopt_root: None,
            spawn_daemon: noop_hook(&spawned),
            spawn_smp: noop_hook(&spawned),
        })
        .await
        .expect("server");
        Self { running, spawned }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.running.addr.port())
    }

    fn addr(&self) -> SocketAddr {
        self.running.addr
    }

    async fn stop(self) {
        self.running.shutdown().await;
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

async fn session(server: &Server) -> (reqwest::Client, String) {
    let http = client();
    let response = http.get(&server.running.url).send().await.unwrap();
    assert_eq!(
        response.status(),
        302,
        "{}",
        response.text().await.unwrap_or_default()
    );
    let header = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    let pair = header.split(';').next().unwrap().to_string();
    assert!(pair.starts_with("hearth_session="));
    (http, pair)
}

fn exchange(addr: SocketAddr, request: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                break
            }
            Err(error) => panic!("{error}"),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn flags_reject_network_bind_and_accept_a_port() {
    let help = parse_web_args(&["--help".to_string()]).unwrap();
    assert!(matches!(help, WebArgs::Help));
    let run = parse_web_args(&[
        "--port".to_string(),
        "0".to_string(),
        "--no-open".to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
    ])
    .unwrap();
    assert!(matches!(
        run,
        WebArgs::Run {
            port: 0,
            open_browser: false
        }
    ));
    let bare = parse_web_args(&[]).unwrap();
    assert!(matches!(
        bare,
        WebArgs::Run {
            port: DEFAULT_PORT,
            open_browser: true
        }
    ));
    let rejected = parse_web_args(&["--host".to_string(), "0.0.0.0".to_string()]).unwrap_err();
    assert!(rejected.contains("0.0.0.0"), "{rejected}");
    assert!(parse_web_args(&["--host".to_string(), "localhost".to_string()]).is_err());
    assert!(parse_web_args(&["--port".to_string(), "nope".to_string()]).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_becomes_a_cookie_and_the_api_stays_on_loopback() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let server = Server::start(file.path()).await;
    let http = client();
    let anonymous = http.get(server.origin()).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    assert!(anonymous.text().await.unwrap().contains("hearthd web"));

    let wrong = http
        .get(format!("{}/?token=nope", server.origin()))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let (http, cookie) = session(&server).await;
    let page = http
        .get(server.origin())
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("Hearth"), "{html}");
    let script_path = html.split("src=\"").nth(1).unwrap().split('"').next().unwrap();
    let script = http.get(format!("{}{script_path}", server.origin())).send().await.unwrap();
    assert_eq!(script.status(), 200);
    assert!(script.text().await.unwrap().contains("Trust this folder"));

    let api = http
        .get(format!("{}/api/workspaces", server.origin()))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(api.status(), 200);
    let body: serde_json::Value = api.json().await.unwrap();
    assert_eq!(body["workspaces"], serde_json::json!([]));

    let refused = exchange(server.addr(), &format!("GET /api/workspaces HTTP/1.1\r\nHost: evil.example\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"));
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
    let open_asset = exchange(
        server.addr(),
        "GET /assets/app.js HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    );
    assert!(open_asset.starts_with("HTTP/1.1 403"), "{open_asset}");

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspaces_round_trip_and_an_untrusted_folder_does_not_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let workspace_file = dir.path().join("workspaces.json");
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("hearth.yaml"), "version: 1\nservices:\n  sample:\n    run: { argv: [\"true\"] }\n    readiness: { kind: exit }\n").unwrap();

    let server = Server::start(&workspace_file).await;
    let (http, cookie) = session(&server).await;
    let origin = server.origin();

    let relative = http
        .post(format!("{origin}/api/workspaces"))
        .header("cookie", &cookie)
        .header("origin", &origin)
        .json(&serde_json::json!({ "path": "relative/folder" }))
        .send()
        .await
        .unwrap();
    assert_eq!(relative.status(), 400);

    let created = http
        .post(format!("{origin}/api/workspaces"))
        .header("cookie", &cookie)
        .header("origin", &origin)
        .json(&serde_json::json!({ "path": project }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let created: serde_json::Value = created.json().await.unwrap();
    assert_eq!(created["created"], true);
    assert_eq!(created["workspace"]["trusted"], false);
    let id = created["workspace"]["id"].as_str().unwrap();
    let added_at = std::fs::read_to_string(&workspace_file).unwrap();
    assert!(added_at.contains("\"trusted\": false"), "{added_at}");
    assert!(has_iso_timestamp(&added_at), "{added_at}");

    let again = http
        .post(format!("{origin}/api/workspaces"))
        .header("cookie", &cookie)
        .header("origin", &origin)
        .json(&serde_json::json!({ "path": project }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 200);
    let again: serde_json::Value = again.json().await.unwrap();
    assert_eq!(again["workspace"]["id"], id);
    assert_eq!(again["created"], false);

    let snapshot = http
        .get(format!("{origin}/api/workspaces/{id}/snapshot"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snapshot.status(), 200);
    let snapshot: serde_json::Value = snapshot.json().await.unwrap();
    assert_eq!(snapshot["trusted"], false);
    assert_eq!(snapshot["catalog"]["services"][0]["id"], "sample");
    assert_eq!(snapshot["services"], serde_json::json!([]));
    assert!(snapshot["configRevision"].as_u64().is_some(), "{snapshot}");
    let missing_reveal = http
        .post(format!("{origin}/api/workspaces/MISSING/reveal"))
        .header("cookie", &cookie)
        .header("origin", &origin)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_reveal.status(), 404);
    assert!(
        !server.spawned.load(Ordering::SeqCst),
        "an untrusted workspace must not start a daemon"
    );

    let trusted = http
        .post(format!("{origin}/api/workspaces/{id}/trust"))
        .header("cookie", &cookie)
        .header("origin", &origin)
        .send()
        .await
        .unwrap();
    assert_eq!(trusted.status(), 200);
    assert!(
        !server.spawned.load(Ordering::SeqCst),
        "trust records the choice and does not spawn by itself"
    );

    let removed = http
        .delete(format!("{origin}/api/workspaces/{id}"))
        .header("cookie", &cookie)
        .header("origin", &origin)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 200);
    let listed = http
        .get(format!("{origin}/api/workspaces"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let listed: serde_json::Value = listed.json().await.unwrap();
    assert_eq!(listed["workspaces"], serde_json::json!([]));

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_workspace_file_is_moved_aside() {
    let dir = tempfile::tempdir().unwrap();
    let workspace_file = dir.path().join("workspaces.json");
    std::fs::write(&workspace_file, b"not json").unwrap();
    let server = Server::start(&workspace_file).await;
    let (http, cookie) = session(&server).await;
    let listed = http
        .get(format!("{}/api/workspaces", server.origin()))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let listed: serde_json::Value = listed.json().await.unwrap();
    assert_eq!(listed["workspaces"], serde_json::json!([]));
    let message = listed["loadError"].as_str().unwrap();
    assert!(message.contains("moved"), "{message}");
    assert!(!workspace_file.exists());
    let aside = std::fs::read_dir(dir.path()).unwrap().find_map(|entry| {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        name.contains("corrupt").then_some(name)
    });
    assert!(aside.is_some(), "corrupt file should be quarantined");
    server.stop().await;
}

fn has_iso_timestamp(text: &str) -> bool {
    text.split('"').any(|part| {
        part.len() == 20
            && part.as_bytes().get(10) == Some(&b'T')
            && part.ends_with('Z')
            && !part.contains('.')
    })
}
