use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use serde_json::{Value, json};

use super::definitions::{self, UserAgent};
use super::json::Json;
use super::settings::HookAgent;
use super::*;
use crate::test_support::TempDir;

fn context(root: &TempDir) -> Context {
    Context::from_inputs(
        Some(root.path().as_os_str().to_owned()),
        root.path().join("bin").into_os_string(),
        root.path().join("delux"),
        root.path().join("config/delux/agents"),
        |_| None,
    )
    .unwrap()
}

fn source() -> Value {
    json!({"name": "My Tool", "executable": "mytool", "file": "~/.mytool/settings.json", "events": {"Start": "clear", "Stop": "done"}})
}

fn parse(root: &TempDir, id: &str, value: &Value) -> Result<UserAgent, String> {
    definitions::parse(id, &value.to_string(), Some(root.path()))
}

fn invoke(context: &Context, args: &[&str]) -> (i32, String, String) {
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let args = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
    let result = run_in(context, &args, &mut output, &mut errors);
    let code = finish(result, &mut errors);
    (
        code,
        String::from_utf8(output).unwrap(),
        String::from_utf8(errors).unwrap(),
    )
}

fn put_definition(context: &Context, id: &str, value: &Value) {
    fs::create_dir_all(&context.definitions_dir).unwrap();
    fs::write(
        context.definitions_dir.join(format!("{id}.json")),
        value.to_string(),
    )
    .unwrap();
}

#[test]
fn reporter_labels_are_validated_and_do_not_change_actions() {
    fn options(args: &[&str]) -> Result<crate::options::Options, String> {
        crate::options::parse_options(&args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>())
    }
    for name in [
        "a",
        "0",
        "my-tool",
        "a-",
        "01234567890123456789012345678901",
    ] {
        for (operation, expected) in [
            (vec!["set", "working"], reporter::Action::Set("working")),
            (vec!["set", "waiting"], reporter::Action::Set("waiting")),
            (vec!["set", "blocked"], reporter::Action::Set("waiting")),
            (vec!["set", "done"], reporter::Action::Set("done")),
            (vec!["set", "error"], reporter::Action::Set("error")),
            (vec!["clear"], reporter::Action::Clear),
        ] {
            let mut args = vec!["agent-status"];
            args.extend(operation);
            args.extend(["--agent", name]);
            let parsed = options(&args).unwrap();
            let arguments = parsed
                .agent_args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            assert_eq!(reporter::action(&arguments, &Value::Null), Some(expected));
            assert_eq!(
                reporter::command(Some("%7"), expected, 1),
                reporter::command(
                    Some("%7"),
                    reporter::action(&arguments, &Value::Null).unwrap(),
                    1
                )
            );
        }
    }
    for name in [
        "",
        "-a",
        "Upper",
        "under_score",
        "has space",
        "a\n",
        "東京",
        "012345678901234567890123456789012",
    ] {
        assert!(
            options(&["agent-status", "set", "working", "--agent", name]).is_err(),
            "{name:?}"
        );
        assert!(
            options(&["agent-status", "clear", "--agent", name]).is_err(),
            "{name:?}"
        );
    }
    for args in [
        vec!["agent-status", "clear", "--agent"],
        vec![
            "agent-status",
            "set",
            "done",
            "--agent",
            "a",
            "--agent",
            "b",
        ],
        vec!["agents", "status", "--agent", "a"],
        vec!["list", "--agent", "a"],
    ] {
        assert!(options(&args).is_err());
    }
    assert_eq!(
        options(&["agent-status", "clear", "--agent=my-tool"])
            .unwrap()
            .agent_args,
        ["clear", "--agent", "my-tool"]
    );
    assert_eq!(
        reporter::action(
            &["hook", "claude", "Stop", "--agent", "custom"],
            &Value::Null
        ),
        None
    );
}

#[test]
fn definition_validation_rejects_every_invalid_field_shape_and_limit() {
    let root = TempDir::new();
    for id in [
        "",
        "-a",
        "A",
        "a_b",
        "a b",
        "a/other",
        "東京",
        "012345678901234567890123456789012",
    ] {
        assert!(
            parse(&root, id, &source())
                .unwrap_err()
                .contains("definition id")
        );
    }
    for agent in AGENTS {
        let error = parse(&root, agent.name, &source()).unwrap_err();
        assert!(error.contains(agent.name) && error.contains("conflicts"));
    }
    for raw in ["", "not JSON", "[]", "null", "42"] {
        assert!(definitions::parse("custom", raw, Some(root.path())).is_err());
    }
    let cases = [
        ("name", Value::Null),
        ("name", json!("")),
        ("name", json!("x".repeat(65))),
        ("executable", Value::Null),
        ("executable", json!("")),
        ("executable", json!("/bin/tool")),
        ("executable", json!("some/tool")),
        ("executable", json!(".")),
        ("executable", json!("..")),
        ("executable", json!("tool\n")),
        ("file", Value::Null),
        ("file", json!("relative.json")),
        ("file", json!("~other/settings.json")),
        ("file", json!("")),
        ("events", Value::Null),
        ("events", json!([])),
        ("events", json!({})),
        ("events", json!({"": "done"})),
        ("events", json!({"bad\nname": "done"})),
        ("events", json!({"Start": "unknown"})),
        ("events", json!({"Start": "idle"})),
        ("events", json!({"Start": true})),
        ("events", json!({"Start": {}})),
        ("events", json!({"Start": {"state": 3}})),
        (
            "events",
            json!({"Start": {"state": "working", "matcher": 3}}),
        ),
        (
            "events",
            json!({"Start": {"state": "working", "typo": true}}),
        ),
        ("handler", Value::Null),
        ("handler", json!([])),
        ("handler", json!({"type": "command"})),
        ("handler", json!({"command": "other"})),
        ("handler", json!({"timeout": null})),
        ("handler", json!({"extra": []})),
        ("handler", json!({"extra": {}})),
        ("typo", json!(true)),
    ];
    for (field, invalid) in cases {
        let mut value = source();
        value[field] = invalid;
        assert!(
            parse(&root, "custom", &value).is_err(),
            "accepted {field}: {}",
            value[field]
        );
    }
    for field in ["name", "file", "events"] {
        let mut value = source();
        value.as_object_mut().unwrap().remove(field);
        assert!(parse(&root, "custom", &value).is_err());
    }
    let mut value = source();
    value["events"] = Value::Object([("x".repeat(65), json!("done"))].into_iter().collect());
    assert!(parse(&root, "custom", &value).is_err());
    value["events"] = Value::Object(
        (0..33)
            .map(|index| (format!("Event{index}"), json!("done")))
            .collect(),
    );
    assert!(parse(&root, "custom", &value).unwrap_err().contains("1–32"));
    value["events"].as_object_mut().unwrap().remove("Event32");
    assert!(parse(&root, "custom", &value).is_ok());
    let mut raw = source().to_string();
    raw.push_str(&" ".repeat(64 * 1024 - raw.len()));
    assert!(definitions::parse("custom", &raw, Some(root.path())).is_ok());
    raw.push(' ');
    assert!(
        definitions::parse("custom", &raw, Some(root.path()))
            .unwrap_err()
            .contains("64 KiB")
    );
}

#[test]
fn definitions_preserve_event_order_expand_home_and_allow_scalar_metadata() {
    let root = TempDir::new();
    let raw = r#"{"name":"東京","file":"~/.tool/hooks.json","events":{"ZLast":"done","AFirst":"clear","Prompt":{"state":"waiting","matcher":"ToolPermission"},"Working":"working"},"handler":{"name":"delux status","timeout":5000,"enabled":true,"ratio":1.5}}"#;
    let agent = definitions::parse("custom", raw, Some(root.path())).unwrap();
    assert_eq!(agent.file, root.path().join(".tool/hooks.json"));
    assert!(agent.executable.is_none());
    assert_eq!(
        agent
            .events
            .iter()
            .map(|event| event.name.as_str())
            .collect::<Vec<_>>(),
        ["ZLast", "AFirst", "Prompt", "Working"]
    );
    assert_eq!(agent.events[2].matcher.as_deref(), Some("ToolPermission"));
    settings::update(&agent, &agent.file, "'/dev/delux'", true).unwrap();
    let installed: Json = serde_json::from_slice(&fs::read(&agent.file).unwrap()).unwrap();
    let hooks = installed.get("hooks").unwrap().as_object().unwrap();
    assert_eq!(
        hooks
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["ZLast", "AFirst", "Prompt", "Working"]
    );
    let value: Value = serde_json::from_slice(&fs::read(&agent.file).unwrap()).unwrap();
    assert_eq!(
        value["hooks"]["ZLast"][0]["hooks"][0],
        json!({"type":"command", "command":"'/dev/delux' agent-status set done --agent custom", "name":"delux status", "timeout":5000, "enabled":true, "ratio":1.5})
    );
    assert_eq!(
        value["hooks"]["AFirst"][0]["hooks"][0]["command"],
        "'/dev/delux' agent-status clear --agent custom"
    );
    assert_eq!(value["hooks"]["Prompt"][0]["matcher"], "ToolPermission");
    let mut value = source();
    value["name"] = json!("東".repeat(64));
    value["events"] = Value::Object([("東".repeat(64), json!("done"))].into_iter().collect());
    assert!(parse(&root, "custom", &value).is_ok());
    assert!(
        definitions::parse("custom", raw, None)
            .unwrap_err()
            .contains("$HOME")
    );
    value["file"] = json!(root.path().join("absolute.json"));
    assert!(definitions::parse("custom", &value.to_string(), None).is_ok());
}

#[test]
fn definitions_accept_reporter_states_and_normalize_blocked() {
    let root = TempDir::new();
    for (state, operation) in [
        ("working", "set working"),
        ("waiting", "set waiting"),
        ("blocked", "set waiting"),
        ("done", "set done"),
        ("error", "set error"),
        ("clear", "clear"),
    ] {
        for event in [
            json!(state),
            json!({"state": state, "matcher": "Permission"}),
        ] {
            let mut value = source();
            value["events"] = json!({"Changed": event});
            let agent = parse(&root, "custom", &value).unwrap();
            let entries = agent.entries("delux");
            assert_eq!(entries.len(), 1);
            assert_eq!(
                entries[0].command,
                format!("delux agent-status {operation} --agent custom"),
            );
        }
    }
}

#[test]
fn ownership_and_lifecycle_preserve_other_agents_and_other_programs() {
    let root = TempDir::new();
    let a = parse(&root, "custom", &source()).unwrap();
    let b = parse(&root, "another", &source()).unwrap();
    let path = &a.file;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = json!({"z": true, "hooks": {"Stop": [{"matcher":"user", "hooks":[{"type":"command", "command":"other-program"}]}]}, "a": []});
    fs::write(path, original.to_string()).unwrap();
    settings::update(&AGENTS[0], path, "delux", true).unwrap();
    settings::update(&b, path, "delux", true).unwrap();
    let baseline: Json = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(
        settings::status(&a, path, "delux").unwrap(),
        "not installed"
    );
    assert!(settings::update(&a, path, "delux", true).unwrap().changed);
    assert_eq!(settings::status(&a, path, "delux").unwrap(), "installed");
    assert_eq!(settings::status(&b, path, "delux").unwrap(), "installed");
    assert_eq!(
        settings::status(&AGENTS[0], path, "delux").unwrap(),
        "installed"
    );
    let bytes = fs::read(path).unwrap();
    let inode = fs::metadata(path).unwrap().ino();
    assert!(!settings::update(&a, path, "delux", true).unwrap().changed);
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(fs::metadata(path).unwrap().ino(), inode);
    assert_eq!(
        settings::status(&a, path, "'/new/delux'").unwrap(),
        "partly installed"
    );
    settings::update(&a, path, "'/new/delux'", true).unwrap();
    settings::update(&a, path, "delux", false).unwrap();
    assert_eq!(
        serde_json::from_slice::<Json>(&fs::read(path).unwrap()).unwrap(),
        baseline
    );
    settings::update(&a, path, "delux", true).unwrap();
    settings::update(&AGENTS[0], path, "delux", false).unwrap();
    assert_eq!(settings::status(&a, path, "delux").unwrap(), "installed");
    assert_eq!(settings::status(&b, path, "delux").unwrap(), "installed");
    settings::update(&a, path, "delux", false).unwrap();
    settings::update(&b, path, "delux", false).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(),
        original
    );
    for command in [
        "delux agent-status hook claude Stop",
        "delux agent-status set done --agent another",
        "delux agent-status set done --agent custom-other",
        "other-command --agent custom",
        "delux agent-status set done --agent custom ",
    ] {
        assert!(!a.owns(command), "{command}");
    }
    assert!(a.owns("'/old/delux' agent-status set waiting --agent custom"));
    assert!(!AGENTS[0].owns("delux agent-status set done --agent custom"));
}

#[test]
fn definitions_load_independently_and_runtime_errors_do_not_stop_valid_agents() {
    let root = TempDir::new();
    let context = context(&root);
    let mut value = source();
    value["name"] = json!("My\u{1b}[2J Tool");
    put_definition(&context, "custom", &value);
    fs::write(context.definitions_dir.join("broken.json"), "not JSON").unwrap();
    fs::create_dir_all(root.path().join(".mytool")).unwrap();
    fs::create_dir_all(&context.directories[0]).unwrap();
    let (code, output, errors) = invoke(&context, &["install"]);
    assert_eq!(code, 1, "{errors}");
    assert!(output.contains("broken.json (invalid user-defined agent):"));
    assert!(output.contains("Claude Code: installed"));
    assert!(output.contains("My\\x1b[2J Tool (user-defined): installed"));
    assert!(!output.contains('\u{1b}'));
    assert!(
        output.find("Claude Code: installed").unwrap()
            < output.find("(user-defined): installed").unwrap()
    );
    assert!(context.file(0).exists());
    let (code, output, _) = invoke(&context, &["status"]);
    assert_eq!(code, 1);
    assert!(output.contains("(user-defined): found; reporting installed"));
    let (code, output, _) = invoke(&context, &["uninstall"]);
    assert_eq!(code, 1);
    assert!(output.contains("(user-defined): uninstalled"));
    let (code, _, errors) = invoke(&context, &["install", "missing"]);
    assert_eq!(code, 2);
    assert!(errors.contains("claude, codex, gemini, opencode, pi, omp, custom"));
    fs::remove_file(context.definitions_dir.join("broken.json")).unwrap();
    let (code, output, _) = invoke(&context, &["install", "custom"]);
    assert_eq!(code, 0);
    assert!(!output.contains("Claude Code:"));
    let (code, output, _) = invoke(&context, &["install", "custom"]);
    assert_eq!(code, 0);
    assert_eq!(output.lines().count(), 1);
    assert!(output.contains("already installed"));
}

#[test]
fn definition_files_are_bounded_and_conflicts_and_invalid_stems_are_reported() {
    let root = TempDir::new();
    let context = context(&root);
    put_definition(&context, "z-tool", &source());
    put_definition(&context, "a-tool", &source());
    put_definition(&context, "gemini", &source());
    put_definition(&context, "Bad_Name", &source());
    fs::write(
        context.definitions_dir.join("huge.json"),
        " ".repeat(64 * 1024 + 1),
    )
    .unwrap();
    fs::write(context.definitions_dir.join("ignored.txt"), "not JSON").unwrap();
    let catalog = definitions::load(&context.definitions_dir, context.home.as_deref());
    assert_eq!(
        catalog
            .agents
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        ["a-tool", "z-tool"]
    );
    assert_eq!(catalog.errors.len(), 3);
    assert!(
        catalog
            .errors
            .iter()
            .any(|(_, error)| error.contains("conflicts"))
    );
    assert!(
        catalog
            .errors
            .iter()
            .any(|(_, error)| error.contains("64 KiB"))
    );
    assert!(
        catalog
            .errors
            .iter()
            .any(|(_, error)| error.contains("definition id"))
    );
}

#[test]
fn user_detection_uses_optional_executable_or_settings_parent() {
    let root = TempDir::new();
    let context = context(&root);
    put_definition(&context, "custom", &source());
    let (code, output, _) = invoke(&context, &["install"]);
    assert_eq!(code, 0);
    assert!(output.contains("No supported agents detected"));
    let (code, _, errors) = invoke(&context, &["install", "custom"]);
    assert_eq!(code, 1);
    assert!(errors.contains("mytool on PATH"));
    fs::create_dir(root.path().join("bin")).unwrap();
    let executable = root.path().join("bin/mytool");
    fs::write(&executable, "never executed").unwrap();
    fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(invoke(&context, &["install", "custom"]).0, 0);
    assert!(root.path().join(".mytool/settings.json").exists());
    let mut value = source();
    value.as_object_mut().unwrap().remove("executable");
    put_definition(&context, "custom", &value);
    assert!(
        invoke(&context, &["status"])
            .1
            .contains("My Tool (user-defined): found")
    );
}

#[test]
fn gemini_uses_parent_directory_override_and_millisecond_timeout() {
    let root = TempDir::new();
    let gemini = AGENTS
        .iter()
        .position(|agent| agent.name == "gemini")
        .unwrap();
    for value in [
        None,
        Some(OsString::new()),
        Some(root.path().join("override").into_os_string()),
    ] {
        let context = Context::from_inputs(
            Some(root.path().as_os_str().to_owned()),
            root.path().join("bin").into_os_string(),
            root.path().join("delux"),
            root.path().join("definitions"),
            |name| {
                if name == "GEMINI_CLI_HOME" {
                    value.clone()
                } else {
                    None
                }
            },
        )
        .unwrap();
        let parent = value
            .as_ref()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| root.path().to_owned());
        assert_eq!(context.file(gemini), parent.join(".gemini/settings.json"));
    }
    let path = root.path().join("settings.json");
    settings::update(&AGENTS[gemini], &path, "delux", true).unwrap();
    let installed: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(
        installed["hooks"]["Notification"][0]["matcher"],
        "ToolPermission"
    );
    for groups in installed["hooks"].as_object().unwrap().values() {
        assert_eq!(groups[0]["hooks"][0]["timeout"], 5000);
    }
}

#[test]
fn user_upgrades_remove_old_paths_and_events_and_detect_missing_events() {
    let root = TempDir::new();
    let agent = parse(&root, "custom", &source()).unwrap();
    fs::create_dir_all(agent.file.parent().unwrap()).unwrap();
    let original = json!({"hooks": {
        "Dropped": [{"hooks": [{"command": "'/old/delux' agent-status set waiting --agent custom"}]}],
        "Stop": [{"hooks": [{"command": "delux agent-status set done --agent another"}]}]
    }});
    fs::write(&agent.file, original.to_string()).unwrap();
    assert_eq!(
        settings::status(&agent, &agent.file, "delux").unwrap(),
        "partly installed"
    );
    settings::update(&agent, &agent.file, "delux", true).unwrap();
    let mut installed: Value = serde_json::from_slice(&fs::read(&agent.file).unwrap()).unwrap();
    assert!(installed["hooks"].get("Dropped").is_none());
    assert_eq!(installed["hooks"]["Stop"][0], original["hooks"]["Stop"][0]);
    assert!(
        installed["hooks"]["Stop"][1]["hooks"][0]
            .get("timeout")
            .is_none()
    );
    installed["hooks"].as_object_mut().unwrap().remove("Start");
    fs::write(&agent.file, installed.to_string()).unwrap();
    assert_eq!(
        settings::status(&agent, &agent.file, "delux").unwrap(),
        "partly installed"
    );
    settings::update(&agent, &agent.file, "delux", true).unwrap();
    assert_eq!(
        settings::status(&agent, &agent.file, "delux").unwrap(),
        "installed"
    );
    settings::update(&agent, &agent.file, "delux", false).unwrap();
    let remaining: Value = serde_json::from_slice(&fs::read(&agent.file).unwrap()).unwrap();
    assert_eq!(
        remaining,
        json!({"hooks": {"Stop": original["hooks"]["Stop"]}})
    );
}
