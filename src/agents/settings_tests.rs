use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::json::Json;
use super::*;
use crate::test_support::TempDir;

fn ordered(raw: &str) -> Json {
    serde_json::from_str(raw).unwrap()
}

fn names(path: &Path) -> Vec<String> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn context(root: &TempDir) -> Context {
    Context::from_inputs(
        Some(root.path().as_os_str().to_owned()),
        root.path().join("bin").into_os_string(),
        root.path().join("delux"),
        |_| None,
    )
    .unwrap()
}

fn invoke(context: &Context, args: &[&str]) -> (Result<i32, String>, String) {
    let mut output = Vec::new();
    let result = run_in(
        context,
        &args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>(),
        &mut output,
    );
    (result, String::from_utf8(output).unwrap())
}

#[test]
fn ordered_json_round_trips_all_values_and_duplicate_nested_keys() {
    let raw = r#"{"z":null,"a":[true,false,-9223372036854775808,18446744073709551615,1.25,1e30,"quote\" backslash\\ newline\n 東京 \u263a",{},[]],"z":{"last":1,"first":2,"last":3}}"#;
    let value = ordered(raw);
    let pretty = serde_json::to_string_pretty(&value).unwrap();
    assert!(pretty.starts_with("{\n  \"z\": null,\n  \"a\": ["));
    assert_eq!(ordered(&pretty), value);
    let entries = value.as_object().unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["z", "a", "z"]
    );
    let nested = value.get("z").unwrap();
    assert_eq!(
        nested
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["last", "first", "last"]
    );
    assert_eq!(nested.get("last"), Some(&Json::Number(3.into())));
    // The private representation must not change maps used by discovery.
    let ordinary: serde_json::Value = serde_json::from_str(r#"{"z":1,"a":2}"#).unwrap();
    assert_eq!(
        ordinary
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["a", "z"]
    );
}

#[test]
fn duplicate_hooks_and_events_keep_entries_and_use_last_lookup() {
    let root = TempDir::new();
    let path = root.path().join("settings.json");
    let raw = r#"{"hooks":{"ignored":"shadowed"},"other":{"b":1,"a":2,"b":3},"hooks":{"Stop":[{"hooks":[{"command":"first user hook"}]}],"Stop":[{"hooks":[{"command":"last user hook"}]}]}}"#;
    let original = ordered(raw);
    fs::write(&path, raw).unwrap();
    settings::update(&AGENTS[0], &path, "delux", true).unwrap();
    let installed = ordered(&fs::read_to_string(&path).unwrap());
    assert_eq!(
        installed.as_object().unwrap()[0],
        original.as_object().unwrap()[0]
    );
    assert_eq!(installed.get("other"), original.get("other"));
    let events = installed.get("hooks").unwrap().as_object().unwrap();
    assert_eq!(
        events[0],
        original.get("hooks").unwrap().as_object().unwrap()[0]
    );
    assert_eq!(events.iter().filter(|(key, _)| key == "Stop").count(), 2);
    assert_eq!(
        installed
            .get("hooks")
            .unwrap()
            .get("Stop")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        settings::status(&AGENTS[0], &path, "delux").unwrap(),
        "installed"
    );
    assert!(
        !settings::update(&AGENTS[0], &path, "delux", true)
            .unwrap()
            .changed
    );
    settings::update(&AGENTS[0], &path, "delux", false).unwrap();
    assert_eq!(ordered(&fs::read_to_string(path).unwrap()), original);
}

#[test]
fn unchanged_settings_keep_bytes_inode_and_directory_contents() {
    let root = TempDir::new();
    let path = root.path().join("settings.json");
    let raw = "{ \"z\": [], \"a\": {}, \"z\": true }\n";
    fs::write(&path, raw).unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    let update = settings::update(&AGENTS[0], &path, "delux", false).unwrap();
    assert!(!update.changed);
    assert!(update.backup.is_none());
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::read_to_string(&path).unwrap(), raw);
    assert_eq!(names(root.path()), ["settings.json"]);
    settings::update(&AGENTS[0], &path, "delux", true).unwrap();
    let installed = fs::read(&path).unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    assert!(
        !settings::update(&AGENTS[0], &path, "delux", true)
            .unwrap()
            .changed
    );
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::read(&path).unwrap(), installed);
    assert_eq!(
        names(root.path()),
        ["settings.json", "settings.json.delux-backup"]
    );
}

#[test]
fn installations_leave_only_settings_backups_or_the_extension() {
    for agent in &AGENTS[..2] {
        let root = TempDir::new();
        let Integration::Hooks { file, .. } = agent.integration else {
            unreachable!()
        };
        let path = root.path().join(file);
        assert!(
            settings::update(agent, &path, "delux", true)
                .unwrap()
                .backup
                .is_none()
        );
        assert_eq!(names(root.path()), [file]);
        let update = settings::update(agent, &path, "delux", false).unwrap();
        assert_eq!(
            update.backup,
            Some(root.path().join(format!("{file}.delux-backup")))
        );
        assert_eq!(
            names(root.path()),
            [file.to_owned(), format!("{file}.delux-backup")]
        );
    }
    for agent in &AGENTS[2..] {
        let Integration::Extension { file, adapter } = agent.integration else {
            unreachable!()
        };
        let root = TempDir::new();
        let filename = Path::new(file).file_name().unwrap().to_str().unwrap();
        let path = root.path().join(filename);
        assert!(
            update_extension(&path, adapter, true)
                .unwrap()
                .backup
                .is_none()
        );
        assert_eq!(names(root.path()), [filename]);
        fs::write(&path, format!("{ADAPTER_MARKER}\n// old version\n")).unwrap();
        assert!(
            update_extension(&path, adapter, true)
                .unwrap()
                .backup
                .is_none()
        );
        assert_eq!(names(root.path()), [filename]);
        assert!(
            update_extension(&path, adapter, false)
                .unwrap()
                .backup
                .is_none()
        );
        assert!(names(root.path()).is_empty());
    }
}

#[test]
fn uninstalling_an_extension_symlink_leaves_no_discovery_entry() {
    for agent in &AGENTS[2..] {
        let Integration::Extension { file, adapter } = agent.integration else {
            unreachable!()
        };
        let root = TempDir::new();
        let extensions = root.path().join("extensions");
        fs::create_dir(&extensions).unwrap();
        let target = root.path().join("source");
        fs::write(&target, adapter).unwrap();
        let link = extensions.join(Path::new(file).file_name().unwrap());
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(update_extension(&link, adapter, false).unwrap().changed);
        assert!(names(&extensions).is_empty());
        assert_eq!(fs::read_to_string(target).unwrap(), adapter);
    }
}

#[test]
fn directory_that_stays_busy_fails_after_a_bounded_wait_without_sidecars() {
    let root = TempDir::new();
    let lock = fs::File::open(root.path()).unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    let error = settings::update(
        &AGENTS[0],
        &root.path().join("settings.json"),
        "delux",
        true,
    )
    .unwrap_err();
    assert!(error.contains("cannot lock directory"));
    assert!(names(root.path()).is_empty());
}

#[test]
fn settings_allow_large_permission_lists_but_refuse_over_one_mib() {
    let root = TempDir::new();
    let path = root.path().join("settings.json");
    let raw = format!("{{\"permissions\":\"{}\"}}", "x".repeat(80 * 1024));
    fs::write(&path, &raw).unwrap();
    assert!(
        settings::update(&AGENTS[0], &path, "delux", true)
            .unwrap()
            .changed
    );
    assert!(fs::metadata(&path).unwrap().len() > 64 * 1024);
    fs::remove_file(root.path().join("settings.json.delux-backup")).unwrap();
    let raw = format!(
        "{{\"permissions\":\"{}\"}}",
        "x".repeat(storage::FILE_LIMIT)
    );
    fs::write(&path, &raw).unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    for install in [true, false] {
        let error = settings::update(&AGENTS[0], &path, "delux", install).unwrap_err();
        assert!(error.contains("exceeds the 1 MiB"));
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::read_to_string(&path).unwrap(), raw);
        assert_eq!(names(root.path()), ["settings.json"]);
    }
    let raw = format!(
        "{{\"permissions\":\"{}\"}}",
        "x".repeat(storage::FILE_LIMIT - 20)
    );
    fs::write(&path, &raw).unwrap();
    assert!(
        settings::update(&AGENTS[0], &path, "delux", true)
            .unwrap_err()
            .contains("would exceed")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), raw);
    assert_eq!(names(root.path()), ["settings.json"]);
}

#[test]
fn output_is_quiet_on_noop_and_mentions_backups_only_when_created() {
    let root = TempDir::new();
    let context = context(&root);
    for directory in &context.directories {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(context.file(0), "{}").unwrap();
    fs::write(context.file(1), "{}").unwrap();
    let (result, output) = invoke(&context, &["install"]);
    assert_eq!(result, Ok(0));
    assert_eq!(output.matches("Command:").count(), 2);
    assert_eq!(output.matches("Backup:").count(), 2);
    assert!(
        output.contains(
            context
                .file(0)
                .with_file_name("settings.json.delux-backup")
                .to_str()
                .unwrap()
        )
    );
    assert!(output.contains("/hooks once"));
    assert_eq!(output.matches("Restart already-running").count(), 1);
    let (result, output) = invoke(&context, &["install"]);
    assert_eq!(result, Ok(0));
    assert_eq!(output.lines().count(), AGENTS.len());
    assert!(
        output
            .lines()
            .all(|line| line.contains("already installed"))
    );
    let (result, output) = invoke(&context, &["uninstall"]);
    assert_eq!(result, Ok(0));
    assert_eq!(output.lines().count(), AGENTS.len());
    assert!(!output.contains("Backup:"));
    assert!(names(context.file(2).parent().unwrap()).is_empty());
}

#[test]
fn failures_are_returned_after_remaining_agents_are_processed() {
    for install in [true, false] {
        let root = TempDir::new();
        let context = context(&root);
        for directory in &context.directories {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(context.file(0), "invalid JSON").unwrap();
        fs::create_dir_all(context.file(2).parent().unwrap()).unwrap();
        fs::write(context.file(2), "unmarked user extension").unwrap();
        if !install {
            settings::update(&AGENTS[1], &context.file(1), "delux", true).unwrap();
        }
        let (result, output) = invoke(
            &context,
            &[
                if install { "install" } else { "uninstall" },
                "claude",
                "codex",
                "opencode",
            ],
        );
        let mut errors = Vec::new();
        assert_eq!(finish(result, &mut errors), 1);
        let error = String::from_utf8(errors).unwrap();
        assert!(error.contains("invalid JSON"));
        assert!(error.contains("refusing"));
        assert!(output.contains(if install {
            "Codex: installed"
        } else {
            "Codex: uninstalled"
        }));
        assert_eq!(
            settings::status(&AGENTS[1], &context.file(1), &context.command().unwrap()).unwrap(),
            if install {
                "installed"
            } else {
                "not installed"
            }
        );
        assert_eq!(fs::read_to_string(context.file(0)).unwrap(), "invalid JSON");
        assert_eq!(
            fs::read_to_string(context.file(2)).unwrap(),
            "unmarked user extension"
        );
    }
    let root = TempDir::new();
    let context = context(&root);
    fs::create_dir_all(&context.directories[1]).unwrap();
    for operation in ["install", "uninstall"] {
        let (result, output) = invoke(&context, &[operation, "claude", "codex"]);
        let mut errors = Vec::new();
        assert_eq!(finish(result, &mut errors), 1);
        assert!(String::from_utf8(errors).unwrap().contains("was not found"));
        assert!(output.contains("Codex:"));
    }
    let args = ["agents", "install", "unknown"].map(String::from);
    assert!(
        crate::options::parse_options(&args)
            .unwrap_err()
            .contains("valid names: claude, codex, opencode, pi, omp")
    );
    assert_eq!(
        crate::app::run(args.into_iter().map(Into::into).collect()),
        2
    );
}

#[test]
fn shadowed_duplicate_types_are_preserved_and_never_become_effective() {
    let root = TempDir::new();
    let path = root.path().join("settings.json");
    let raw = r#"{"hooks":false,"hooks":{"Stop":"shadowed value","Stop":[]}}"#;
    fs::write(&path, raw).unwrap();
    settings::update(&AGENTS[0], &path, "delux", true).unwrap();
    assert_eq!(
        settings::status(&AGENTS[0], &path, "delux").unwrap(),
        "installed"
    );
    assert!(
        !settings::update(&AGENTS[0], &path, "delux", true)
            .unwrap()
            .changed
    );
    settings::update(&AGENTS[0], &path, "delux", false).unwrap();
    assert_eq!(ordered(&fs::read_to_string(&path).unwrap()), ordered(raw));
    assert_eq!(
        settings::status(&AGENTS[0], &path, "delux").unwrap(),
        "not installed"
    );
}
