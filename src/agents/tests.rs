use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

use serde_json::{Value, json};

use super::*;
use crate::test_support::TempDir;

fn context(root: &TempDir) -> Context {
    Context::from_inputs(
        Some(root.path().as_os_str().to_owned()),
        root.path().join("bin").into_os_string(),
        root.path().join("delux-dev"),
        |_| None,
    )
    .unwrap()
}

fn invoke(context: &Context, args: &[&str]) -> Result<String, String> {
    let mut output = Vec::new();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    assert_eq!(run_in(context, &args, &mut output)?, 0);
    Ok(String::from_utf8(output).unwrap())
}

fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn event_mapping_table_and_background_tasks() {
    use reporter::Action::{Clear, Set};
    let cases = [
        ("claude", "SessionStart", Clear),
        ("claude", "UserPromptSubmit", Set("working")),
        ("claude", "PreToolUse", Set("waiting")),
        ("claude", "PermissionRequest", Set("waiting")),
        ("claude", "PostToolUse", Set("working")),
        ("claude", "PostToolUseFailure", Set("working")),
        ("claude", "Notification", Set("waiting")),
        ("claude", "Stop", Set("done")),
        ("claude", "StopFailure", Set("done")),
        ("claude", "SessionEnd", Clear),
        ("codex", "SessionStart", Clear),
        ("codex", "UserPromptSubmit", Set("working")),
        ("codex", "PermissionRequest", Set("waiting")),
        ("codex", "PostToolUse", Set("working")),
        ("codex", "Stop", Set("done")),
        ("codex", "Interrupt", Clear),
        ("codex", "SessionEnd", Clear),
    ];
    for (agent, event, expected) in cases {
        assert_eq!(
            reporter::mapping(agent, event, &Value::Null),
            Some(expected),
            "{agent} {event}"
        );
    }
    for agent in AGENTS {
        if let Integration::Hooks { events, .. } = agent.integration {
            for event in events {
                assert!(
                    cases
                        .iter()
                        .any(|(name, event_name, _)| *name == agent.name
                            && *event_name == event.name)
                );
            }
        }
    }
    for payload in [
        json!({}),
        json!({"background_tasks": []}),
        json!({"background_tasks": "not an array"}),
    ] {
        assert_eq!(
            reporter::mapping("claude", "Stop", &payload),
            Some(Set("done"))
        );
    }
    let payload = json!({"background_tasks": [{"id": 1}]});
    assert_eq!(
        reporter::mapping("claude", "Stop", &payload),
        Some(Set("working"))
    );
    assert_eq!(
        reporter::mapping("codex", "Stop", &payload),
        Some(Set("done"))
    );
    for (agent, event) in [
        ("unknown", "Stop"),
        ("claude", "Unknown"),
        ("codex", "PreToolUse"),
        ("omp", "Stop"),
    ] {
        assert_eq!(reporter::mapping(agent, event, &payload), None);
    }
}

#[test]
fn reporter_bounds_payload_and_constructs_literal_tmux_arguments() {
    use reporter::Action::{Clear, Set};
    assert_eq!(reporter::payload(b"not json"), Value::Null);
    assert_eq!(reporter::payload(&vec![b' '; 1024 * 1024 + 1]), Value::Null);
    assert_eq!(
        reporter::payload(b"{\"background_tasks\":[1]}"),
        json!({"background_tasks": [1]})
    );
    assert_eq!(reporter::command(None, Clear, 10), None);
    assert_eq!(reporter::command(Some(""), Set("done"), 10), None);
    assert_eq!(
        reporter::command(Some("%3"), Clear, 10).unwrap(),
        [
            "tmux",
            "set-option",
            "-p",
            "-u",
            "-t",
            "%3",
            "@agent_status"
        ]
    );
    assert_eq!(
        reporter::command(Some("%3; echo unsafe"), Set("working"), 10).unwrap(),
        [
            "tmux",
            "set-option",
            "-p",
            "-t",
            "%3; echo unsafe",
            "@agent_status",
            "working 10"
        ]
    );
}

#[test]
fn installs_missing_and_empty_documents_with_expected_handlers() {
    for agent in &AGENTS[..2] {
        for original in [None, Some("{}\n"), Some("{\"hooks\": {}}\n")] {
            let root = TempDir::new();
            let path = root.path().join("settings.json");
            if let Some(raw) = original {
                fs::write(&path, raw).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            assert!(
                settings::update(agent, &path, "delux", true)
                    .unwrap()
                    .changed
            );
            let installed = json_file(&path);
            let Integration::Hooks { events, .. } = agent.integration else {
                unreachable!()
            };
            assert_eq!(installed["hooks"].as_object().unwrap().len(), events.len());
            for event in events {
                let groups = installed["hooks"][event.name].as_array().unwrap();
                assert_eq!(groups.len(), 1);
                assert_eq!(
                    groups[0].get("matcher").and_then(Value::as_str),
                    event.matcher
                );
                assert_eq!(
                    groups[0]["hooks"][0],
                    json!({
                        "type": "command", "command": format!("delux agent-status hook {} {}", agent.name, event.name), "timeout": 5,
                    })
                );
            }
            assert_eq!(
                settings::status(agent, &path, "delux").unwrap(),
                "installed"
            );
            assert!(fs::read_to_string(&path).unwrap().ends_with('\n'));
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn preserves_other_settings_handlers_and_key_order_and_uninstalls() {
    let root = TempDir::new();
    let path = root.path().join("settings.json");
    let original = r#"{"z-first": 1, "hooks": {"Stop": [{"matcher":"other","hooks":[{"type":"command","command":"other stop","timeout":17}]}],"Custom": [],"Notification":[{"hooks":[]}]}, "a-last": {"z": 1, "a": 2}}"#;
    fs::write(&path, original).unwrap();
    let original_value: Value = serde_json::from_str(original).unwrap();
    settings::update(&AGENTS[0], &path, "delux", true).unwrap();
    let installed = json_file(&path);
    assert_eq!(
        installed["hooks"]["Stop"][0],
        original_value["hooks"]["Stop"][0]
    );
    assert_eq!(
        installed["hooks"]["Notification"][0],
        original_value["hooks"]["Notification"][0]
    );
    assert_eq!(installed["hooks"]["Custom"], json!([]));
    assert_eq!(installed["a-last"], original_value["a-last"]);
    let ordered: super::json::Json = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        ordered
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["z-first", "hooks", "a-last"]
    );
    assert_eq!(
        ordered
            .get("a-last")
            .unwrap()
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["z", "a"]
    );
    let bytes = fs::read(&path).unwrap();
    assert!(
        !settings::update(&AGENTS[0], &path, "delux", true)
            .unwrap()
            .changed
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(
        settings::update(&AGENTS[0], &path, "delux", false)
            .unwrap()
            .changed
    );
    assert_eq!(json_file(&path), original_value);
    assert!(
        !settings::update(&AGENTS[0], &path, "delux", false)
            .unwrap()
            .changed
    );
    assert_eq!(
        settings::status(&AGENTS[0], &path, "delux").unwrap(),
        "not installed"
    );
}

#[test]
fn upgrades_old_paths_and_events_including_mixed_groups() {
    let root = TempDir::new();
    let path = root.path().join("settings.json");
    let other = json!({"type": "command", "command": "someone else"});
    let other_agent = json!({"type": "command", "command": "delux agent-status hook codex Stop"});
    let original = json!({"hooks": {
        "Dropped": [{"hooks": [{"command": "'/old/delux' agent-status hook claude Dropped"}]}],
        "Stop": [{"matcher": "keep", "extra": true, "hooks": [other.clone(), {"command": "'/old/delux' agent-status hook claude Stop"}, other_agent.clone()]}],
    }});
    fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    assert_eq!(
        settings::status(&AGENTS[0], &path, "delux").unwrap(),
        "partly installed"
    );
    settings::update(&AGENTS[0], &path, "delux", true).unwrap();
    let installed = json_file(&path);
    assert!(installed["hooks"].get("Dropped").is_none());
    assert_eq!(
        installed["hooks"]["Stop"][0],
        json!({"matcher":"keep", "extra":true,"hooks":[other, other_agent]})
    );
    assert_eq!(
        settings::status(&AGENTS[0], &path, "delux").unwrap(),
        "installed"
    );
    assert_eq!(
        settings::status(&AGENTS[0], &path, "'/new/delux'").unwrap(),
        "partly installed"
    );
    settings::update(&AGENTS[0], &path, "'/new/delux'", true).unwrap();
    assert_eq!(
        settings::status(&AGENTS[0], &path, "'/new/delux'").unwrap(),
        "installed"
    );
    let mut missing = json_file(&path);
    missing["hooks"]
        .as_object_mut()
        .unwrap()
        .remove("SessionStart");
    fs::write(&path, serde_json::to_vec(&missing).unwrap()).unwrap();
    assert_eq!(
        settings::status(&AGENTS[0], &path, "'/new/delux'").unwrap(),
        "partly installed"
    );
}

#[test]
fn rejects_malformed_settings_without_touching_bytes_or_backing_up() {
    let root = TempDir::new();
    let path = root.path().join("settings.json");
    for raw in [
        "",
        "not JSON",
        "[]",
        r#"{"hooks": null}"#,
        r#"{"hooks": []}"#,
        r#"{"hooks": {"Stop": {}}}"#,
        r#"{"hooks": {"Stop": [null]}}"#,
        r#"{"hooks": {"Stop": [{"hooks": "bad"}]}}"#,
        r#"{"hooks": {"Stop": [{"hooks": [false]}]}}"#,
    ] {
        fs::write(&path, raw).unwrap();
        for install in [true, false] {
            let error = settings::update(&AGENTS[0], &path, "delux", install).unwrap_err();
            assert!(error.contains(path.to_str().unwrap()));
            assert_eq!(fs::read(&path).unwrap(), raw.as_bytes());
            assert!(!root.path().join("settings.json.delux-backup").exists());
        }
    }
}

#[test]
fn backups_are_created_once_modes_survive_and_links_are_preserved() {
    let root = TempDir::new();
    let target = root.path().join("target.json");
    let link = root.path().join("settings.json");
    let original = b"{\"hooks\":{},\"keep\":true}\n";
    fs::write(&target, original).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&target, &link).unwrap();
    settings::update(&AGENTS[0], &link, "delux", true).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(&link).unwrap(), target);
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let backup = root.path().join("settings.json.delux-backup");
    assert_eq!(fs::read(&backup).unwrap(), original);
    settings::update(&AGENTS[0], &link, "'/new/delux'", true).unwrap();
    settings::update(&AGENTS[0], &link, "delux", false).unwrap();
    assert_eq!(fs::read(&backup).unwrap(), original);
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        json_file(&link),
        serde_json::from_slice::<Value>(original).unwrap()
    );
    let dangling = root.path().join("dangling.json");
    symlink(root.path().join("missing"), &dangling).unwrap();
    assert!(settings::update(&AGENTS[0], &dangling, "delux", true).is_err());
    assert!(
        fs::symlink_metadata(dangling)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn extensions_require_their_marker_and_report_updates() {
    for agent in &AGENTS[2..] {
        let Integration::Extension { file, adapter } = agent.integration else {
            unreachable!()
        };
        let root = TempDir::new();
        let path = root.path().join(Path::new(file).file_name().unwrap());
        assert_eq!(adapter.lines().next(), Some(ADAPTER_MARKER));
        for raw in [
            "user code".to_owned(),
            format!("// header\n{ADAPTER_MARKER}\n"),
        ] {
            fs::write(&path, &raw).unwrap();
            assert_eq!(
                reporting_status(agent, &path, "delux").unwrap(),
                "not installed"
            );
            assert!(
                update_extension(&path, adapter, true)
                    .unwrap_err()
                    .contains("refusing")
            );
            assert!(update_extension(&path, adapter, false).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), raw);
        }
        fs::write(&path, format!("{ADAPTER_MARKER}\n// old adapter\n")).unwrap();
        assert_eq!(
            reporting_status(agent, &path, "delux").unwrap(),
            "installed (update available)"
        );
        assert!(update_extension(&path, adapter, true).unwrap().changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), adapter);
        assert!(!update_extension(&path, adapter, true).unwrap().changed);
        assert_eq!(
            reporting_status(agent, &path, "delux").unwrap(),
            "installed"
        );
        assert!(update_extension(&path, adapter, false).unwrap().changed);
        assert!(!path.exists());
        assert!(!update_extension(&path, adapter, false).unwrap().changed);
        assert!(update_extension(&path, adapter, true).unwrap().changed);
    }
}

#[test]
fn executable_choice_uses_canonical_identity_and_quotes_absolute_paths() {
    let root = TempDir::new();
    let executable = root.path().join("my 'delux");
    let link = root.path().join("delux");
    let different = root.path().join("other-delux");
    fs::write(&executable, "").unwrap();
    fs::write(&different, "").unwrap();
    symlink(&executable, &link).unwrap();
    assert_eq!(
        handler_executable(&executable, Some(&link)).unwrap(),
        "delux"
    );
    let quoted = format!("'{}/my '\\''delux'", root.path().display());
    assert_eq!(handler_executable(&executable, None).unwrap(), quoted);
    assert_eq!(
        handler_executable(&executable, Some(&different)).unwrap(),
        quoted
    );
    assert!(handler_executable(Path::new("relative"), None).is_err());
}

#[test]
fn all_environment_paths_are_injected_including_omp_overrides() {
    let root = TempDir::new();
    let default = context(&root);
    assert_eq!(default.file(0), root.path().join(".claude/settings.json"));
    assert_eq!(default.file(1), root.path().join(".codex/hooks.json"));
    assert_eq!(
        default.file(4),
        root.path()
            .join(".omp/agent/extensions/delux-agent-status.ts")
    );
    fs::create_dir(root.path().join(".omp")).unwrap();
    assert!(
        default.found(4),
        "the fallback config root is enough to detect OMP"
    );
    let custom = |variables: &[(&str, &str)]| {
        Context::from_inputs(
            Some(root.path().as_os_str().to_owned()),
            default.path.clone(),
            default.executable.clone(),
            |name| {
                variables
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(*value))
            },
        )
        .unwrap()
    };
    let directories = custom(&[
        ("CLAUDE_CONFIG_DIR", "/fake/claude"),
        ("CODEX_HOME", "/fake/codex"),
        ("PI_CODING_AGENT_DIR", "/fake/omp"),
        ("PI_CONFIG_DIR", "ignored"),
    ]);
    assert_eq!(directories.file(0), Path::new("/fake/claude/settings.json"));
    assert_eq!(directories.file(1), Path::new("/fake/codex/hooks.json"));
    assert_eq!(
        directories.file(4),
        Path::new("/fake/omp/extensions/delux-agent-status.ts")
    );
    assert_eq!(
        custom(&[("PI_CONFIG_DIR", ".custom")]).file(4),
        root.path()
            .join(".custom/agent/extensions/delux-agent-status.ts")
    );
    assert_eq!(
        custom(&[
            ("PI_CODING_AGENT_DIR", ""),
            ("PI_CONFIG_DIR", ""),
            ("CLAUDE_CONFIG_DIR", ""),
            ("CODEX_HOME", "")
        ])
        .directories,
        default.directories
    );
}

#[test]
fn cli_detection_defaults_explicit_errors_and_codex_guidance() {
    let root = TempDir::new();
    let context = context(&root);
    assert!(
        invoke(&context, &["install"])
            .unwrap()
            .contains("No supported agents detected")
    );
    assert!(
        invoke(&context, &["uninstall"])
            .unwrap()
            .contains("No Delux agent reporting installed")
    );
    let error = invoke(&context, &["install", "claude"]).unwrap_err();
    assert!(error.contains("was not found"));
    assert!(error.contains(context.directories[0].to_str().unwrap()));
    fs::create_dir_all(root.path().join("bin")).unwrap();
    let codex = root.path().join("bin/codex");
    fs::write(&codex, "not executable code").unwrap();
    assert!(!context.found(1));
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(context.found(1));
    fs::create_dir_all(&context.directories[1]).unwrap();
    fs::write(
        context.directories[1].join("config.toml"),
        "  [[hooks.Stop]]\n",
    )
    .unwrap();
    let output = invoke(&context, &["install"]).unwrap();
    assert!(output.contains("/hooks once"));
    assert!(output.contains("merges both sources"));
    assert!(output.contains("Restart already-running"));
    assert!(output.contains("agent-status hook codex <Event>"));
    let installed = fs::read(context.file(1)).unwrap();
    assert!(
        invoke(&context, &["install", "codex"])
            .unwrap()
            .contains("already installed")
    );
    assert_eq!(fs::read(context.file(1)).unwrap(), installed);
    let status = invoke(&context, &[]).unwrap();
    assert_eq!(status, invoke(&context, &["status"]).unwrap());
    assert!(status.contains("Claude Code: not found; reporting not installed"));
    assert!(status.contains("Codex: found; reporting installed"));
    assert!(
        invoke(&context, &["uninstall"])
            .unwrap()
            .contains("Codex: uninstalled")
    );
    assert_eq!(json_file(&context.file(1)), json!({"hooks": {}}));
}

#[test]
fn argument_parsing_has_no_discovery_or_home_dependencies() {
    fn parse(args: &[&str]) -> Result<crate::options::Options, String> {
        crate::options::parse_options(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }
    for args in [
        vec!["agents"],
        vec!["agents", "status"],
        vec!["agents", "install", "claude", "omp"],
        vec!["agents", "uninstall"],
        vec!["agent-status", "set", "working"],
        vec!["agent-status", "set", "waiting"],
        vec!["agent-status", "set", "done"],
        vec!["agent-status", "clear"],
        vec!["agent-status", "hook", "unknown", "unknown"],
        vec!["agent-status", "hook"],
    ] {
        let options = parse(&args).unwrap();
        assert!(options.config_path.is_empty());
    }
    for args in [
        vec!["agents", "status", "claude"],
        vec!["agents", "install", "unknown"],
        vec!["agents", "unknown"],
        vec!["agent-status"],
        vec!["agent-status", "set", "bad"],
        vec!["agent-status", "set"],
        vec!["agent-status", "clear", "extra"],
    ] {
        assert!(parse(&args).is_err(), "{args:?}");
    }
    let error = parse(&["agents", "install", "unknown"]).unwrap_err();
    assert!(error.contains("claude, codex, opencode, pi, omp"));
    for flags in [
        vec!["--host", "example"],
        vec!["--local"],
        vec!["--no-local"],
        vec!["--json"],
        vec!["--config", "/unused"],
    ] {
        for command in [vec!["agents", "install"], vec!["agent-status", "clear"]] {
            let mut before = flags.clone();
            before.extend(command.clone());
            let mut after = command;
            after.extend(flags.clone());
            assert!(parse(&before).is_err());
            assert!(parse(&after).is_err());
        }
    }
    let mut help = Vec::new();
    crate::options::print_help(&mut help, "");
    let help = String::from_utf8(help).unwrap();
    assert!(help.contains("delux agents install"));
    assert!(help.contains("hook <agent> <event>"));
    assert!(help.starts_with("Usage:"));
    assert!(!help.contains("Agent reporting:"));
}
