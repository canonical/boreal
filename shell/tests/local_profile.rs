#![cfg(feature = "user-shell")]

use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use boreal_common::wire::{self, Request, Response, ResultOutcome, RunInput, WorkflowDetail};

const SSH_VARS: [&str; 4] = [
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "SSH_ORIGINAL_COMMAND",
];

fn write_catalog_file(path: &Path, contents: &str) {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(path)
        .unwrap()
        .write_all(contents.as_bytes())
        .unwrap();
}

struct Profile {
    root: PathBuf,
    home: PathBuf,
    catalog: PathBuf,
    socket: PathBuf,
    unrelated_home: PathBuf,
}

impl Profile {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bs-{}", uuid::Uuid::new_v4().simple()));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let profile = Self {
            home: root.join("profile home"),
            catalog: root.join("catalog"),
            socket: root.join("broker.sock"),
            unrelated_home: root.join("unrelated"),
            root,
        };
        for dir in [&profile.home, &profile.unrelated_home] {
            DirBuilder::new().mode(0o700).create(dir).unwrap();
        }
        let schema_dir = profile.root.join("schema");
        fs::create_dir(&schema_dir).unwrap();
        DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(profile.catalog.join("users/local"))
            .unwrap();
        fs::write(
            schema_dir.join("schema.yaml"),
            "greet:\n  workflow: DemoWorkflow\n  args:\n  - name: name\n    type: string\n",
        )
        .unwrap();
        write_catalog_file(
            &profile.catalog.join("schemas.yaml"),
            &format!("directories:\n  - '{}'\n", schema_dir.display()),
        );
        write_catalog_file(
            &profile.catalog.join("users/local/perms.yaml"),
            "user: local\npermissions: {read: false, write: false}\nworkers:\n  greet:\n    package: greet\n    permissions: {read: false, write: false}\n",
        );
        profile
    }

    fn shell(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_boreal-shell"));
        for name in SSH_VARS {
            cmd.env_remove(name);
        }
        cmd.env("HOME", &self.unrelated_home)
            .env("BOREAL_TEST_HOME", &self.unrelated_home)
            .env("BOREAL_TEST_ETC_DIR", self.root.join("wrong-catalog"))
            .env("BOREAL_TEST_BROKER_SOCKET", self.root.join("wrong.sock"))
            .env("BOREAL_TEST_CALLER", "wrong-caller");
        cmd
    }

    fn local(&self) -> Command {
        let mut cmd = self.shell();
        cmd.arg("--local")
            .arg(&self.home)
            .arg(&self.catalog)
            .arg(&self.socket);
        cmd
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_rejected(output: Output, diagnostic: &str) {
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(diagnostic),
        "{output:?}"
    );
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn direct_workflow_commands_use_local_catalog_and_socket() {
    let profile = Profile::new();
    let listener = UnixListener::bind(&profile.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let json = r#"{"name":"Alice O'Neil & Bob"}"#;
    let detail = WorkflowDetail {
        owner: "local".into(),
        worker: "greet".into(),
        workflow_type: "DemoWorkflow".into(),
        task_queue: "local-greet".into(),
        workflow_id: "local-1".into(),
        run_id: "run-1".into(),
        status: "completed".into(),
        start_time: "-".into(),
        close_time: "-".into(),
    };
    let cases: &[(&[&str], Request, Response, &str)] = &[
        (
            &["run", "greet", "--id", "local-1", "--input-json", json],
            Request::Run {
                owner: "local".into(),
                worker: "greet".into(),
                workflow: None,
                id: "local-1".into(),
                input: RunInput::Json(json.into()),
            },
            Response::Started {
                owner: detail.owner.clone(),
                worker: detail.worker.clone(),
                workflow_type: detail.workflow_type.clone(),
                workflow_id: detail.workflow_id.clone(),
                run_id: detail.run_id.clone(),
            },
            "started DemoWorkflow 'local-1' on local/greet",
        ),
        (
            &["list"],
            Request::List {
                owner: "local".into(),
            },
            Response::List(vec![]),
            "no workflows found.",
        ),
        (
            &["status", "local-1"],
            Request::Status {
                owner: "local".into(),
                id: "local-1".into(),
            },
            Response::Status(detail),
            "owner:       local",
        ),
        (
            &["result", "local-1"],
            Request::Result {
                owner: "local".into(),
                id: "local-1".into(),
            },
            Response::WorkflowResult(ResultOutcome::Completed {
                run_id: "run-1".into(),
                output: "fixture-result".into(),
            }),
            "fixture-result",
        ),
        (
            &["workers"],
            Request::Workers,
            Response::Workers(vec![]),
            "no accessible workers.",
        ),
    ];

    for (argv, request, response, stdout) in cases {
        thread::scope(|scope| {
            let server = scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("expected a connection on the explicit socket: {e}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                assert_eq!(
                    &Request::decode(&wire::read_frame(&mut stream).unwrap()).unwrap(),
                    request
                );
                wire::write_frame(&mut stream, &response.encode()).unwrap();
            });
            let output = profile.local().args(*argv).output().unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(stdout),
                "{output:?}"
            );
            server.join().unwrap();
        });
    }
    assert!(!profile.home.join("boreal.history").exists());
}

#[test]
fn direct_secret_set_uses_only_the_private_profile_home() {
    let profile = Profile::new();
    let fixture = b"synthetic-test-input";
    let mut child = profile
        .local()
        .args(["secrets", "set", "fixture_key"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(fixture).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let secrets_dir = profile.home.join(".config/boreal/secrets");
    let meta = fs::metadata(secrets_dir.join("fixture_key")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(meta.len(), fixture.len() as u64);
    assert_eq!(
        fs::metadata(secrets_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(!profile.unrelated_home.join(".config").exists());
    assert!(!profile.home.join("boreal.history").exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-test-input"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-test-input"));
}

#[test]
fn explicit_local_invocations_fail_closed() {
    let profile = Profile::new();
    for var in SSH_VARS {
        assert_rejected(
            profile
                .local()
                .arg("list")
                .env(var, "list")
                .output()
                .unwrap(),
            "--local is unavailable in SSH sessions",
        );
    }
    assert_rejected(profile.shell().arg("--local").output().unwrap(), "usage:");
    assert_rejected(
        profile.local().output().unwrap(),
        "requires an interactive terminal",
    );
    assert_rejected(
        profile.local().arg("list").output().unwrap(),
        "connect broker",
    );
    assert_rejected(
        profile.local().arg("status").output().unwrap(),
        "specify a workflow ID",
    );
    assert_rejected(
        profile.local().arg("cancel").output().unwrap(),
        "unsupported local command",
    );
    assert_rejected(
        profile
            .local()
            .args(["secrets", "set", "fixture_key", "inline"])
            .output()
            .unwrap(),
        "unsupported local command",
    );
    assert!(!profile.home.join(".config").exists());

    let other_user = profile.catalog.join("users/alice");
    DirBuilder::new().mode(0o700).create(&other_user).unwrap();
    write_catalog_file(
        &other_user.join("perms.yaml"),
        "user: alice\npermissions: {read: false, write: false}\n",
    );
    assert_rejected(
        profile.local().arg("list").output().unwrap(),
        "must contain exactly the 'local' account",
    );

    let linked_home = profile.root.join("linked-home");
    symlink(&profile.home, &linked_home).unwrap();
    assert_rejected(
        profile
            .shell()
            .arg("--local")
            .arg(linked_home.join("."))
            .arg(&profile.catalog)
            .arg(&profile.socket)
            .arg("list")
            .output()
            .unwrap(),
        "non-symlink directory",
    );
    fs::set_permissions(&profile.home, Permissions::from_mode(0o755)).unwrap();
    assert_rejected(
        profile.local().arg("list").output().unwrap(),
        "invalid local profile home",
    );
}
