use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;
use crate::test_support::TempDir;

fn index(name: &str) -> usize {
    AGENTS.iter().position(|agent| agent.name == name).unwrap()
}

fn context(root: &TempDir, variables: &[(&str, &str)]) -> Context {
    Context::from_inputs(
        Some(root.path().as_os_str().to_owned()),
        root.path().join("bin").into_os_string(),
        root.path().join("delux"),
        root.path().join("definitions"),
        |name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| {
                    if value.is_empty()
                        || matches!(name, "PI_CONFIG_DIR" | "OMP_PROFILE" | "PI_PROFILE")
                    {
                        OsString::from(*value)
                    } else {
                        root.path().join(value).into_os_string()
                    }
                })
        },
    )
    .unwrap()
}

fn invoke(context: &Context, args: &[&str]) -> String {
    let mut output = Vec::new();
    let args = args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>();
    assert_eq!(run_in(context, &args, &mut output, &mut Vec::new()), Ok(0));
    String::from_utf8(output).unwrap()
}

#[test]
fn extension_directories_follow_table_data_for_set_empty_and_unset_variables() {
    let root = TempDir::new();
    // Environment variables, then the expected directory for opencode, pi, omp.
    type Case = (&'static [(&'static str, &'static str)], [&'static str; 3]);
    let cases: &[Case] = &[
        (&[], [".config/opencode", ".pi/agent", ".omp/agent"]),
        (
            &[("PI_CODING_AGENT_DIR", "shared")],
            [".config/opencode", "shared", "shared"],
        ),
        (
            &[("PI_CODING_AGENT_DIR", "")],
            [".config/opencode", ".pi/agent", ".omp/agent"],
        ),
        (
            &[("PI_CONFIG_DIR", ".custom")],
            [".config/opencode", ".pi/agent", ".custom/agent"],
        ),
        (
            &[("PI_CONFIG_DIR", "")],
            [".config/opencode", ".pi/agent", ".omp/agent"],
        ),
        (
            &[("OPENCODE_CONFIG_DIR", "open-config")],
            ["open-config", ".pi/agent", ".omp/agent"],
        ),
        (
            &[("OPENCODE_CONFIG_DIR", "")],
            [".config/opencode", ".pi/agent", ".omp/agent"],
        ),
        (
            &[("XDG_CONFIG_HOME", "xdg")],
            ["xdg/opencode", ".pi/agent", ".omp/agent"],
        ),
        (
            &[("XDG_CONFIG_HOME", "")],
            [".config/opencode", ".pi/agent", ".omp/agent"],
        ),
        (
            &[
                ("OPENCODE_CONFIG_DIR", "custom"),
                ("XDG_CONFIG_HOME", "ignored"),
            ],
            ["custom", ".pi/agent", ".omp/agent"],
        ),
        (
            &[("OPENCODE_CONFIG_DIR", ""), ("XDG_CONFIG_HOME", "xdg")],
            ["xdg/opencode", ".pi/agent", ".omp/agent"],
        ),
        (
            &[
                ("PI_CODING_AGENT_DIR", "shared"),
                ("PI_CONFIG_DIR", "ignored"),
            ],
            [".config/opencode", "shared", "shared"],
        ),
        (
            &[("PI_CODING_AGENT_DIR", ""), ("PI_CONFIG_DIR", ".custom")],
            [".config/opencode", ".pi/agent", ".custom/agent"],
        ),
    ];
    for (variables, expected) in cases {
        let context = context(&root, variables);
        for (name, directory) in ["opencode", "pi", "omp"].into_iter().zip(expected) {
            let index = index(name);
            assert_eq!(
                context.directories[index],
                root.path().join(directory),
                "{name}: {variables:?}"
            );
            let relative = if name == "opencode" {
                "plugins/delux-agent-status.js"
            } else {
                "extensions/delux-agent-status.ts"
            };
            assert_eq!(
                context.file(index),
                root.path().join(directory).join(relative)
            );
        }
    }
}

#[test]
fn fallback_root_detection_is_data_driven_and_overrides_do_not_inherit_it() {
    let root = TempDir::new();
    fs::create_dir(root.path().join(".omp")).unwrap();
    let defaults = context(&root, &[]);
    assert!(defaults.found(index("omp")));
    assert!(!defaults.found(index("pi")));
    assert!(!defaults.found(index("opencode")));
    fs::create_dir(root.path().join("shared")).unwrap();
    let override_path = context(&root, &[("PI_CODING_AGENT_DIR", "shared/agent")]);
    assert!(!override_path.found(index("omp")));
    assert!(!override_path.found(index("pi")));
    fs::create_dir(root.path().join("shared/agent")).unwrap();
    assert!(override_path.found(index("omp")));
    assert!(override_path.found(index("pi")));
}

#[test]
fn extension_executables_are_detected_without_running_them() {
    let root = TempDir::new();
    let context = context(&root, &[]);
    fs::create_dir(root.path().join("bin")).unwrap();
    for name in ["opencode", "pi", "omp"] {
        assert!(!context.found(index(name)));
        let executable = root.path().join("bin").join(name);
        fs::write(&executable, "fixture, never executed").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(context.found(index(name)));
    }
}

#[test]
fn shared_pi_and_omp_directory_has_one_adapter_and_one_explanation() {
    for requested in [["pi", "omp"], ["omp", "pi"]] {
        let root = TempDir::new();
        let context = context(&root, &[("PI_CODING_AGENT_DIR", "shared")]);
        fs::create_dir(&context.directories[index("pi")]).unwrap();
        let path = context.file(index("pi"));
        assert_eq!(path, context.file(index("omp")));
        let install = ["install", requested[0], requested[1]];
        let output = invoke(&context, &install);
        assert_eq!(output.matches("share one adapter").count(), 1);
        assert_eq!(output.matches(": installed").count(), 2);
        assert!(output.find("Pi: installed").unwrap() < output.find("OMP: installed").unwrap());
        let bytes = fs::read(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        assert_eq!(
            bytes.as_slice(),
            include_bytes!("../../adapters/pi/delux-agent-status.ts").as_slice()
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        let output = invoke(&context, &install);
        assert_eq!(output.matches("share one adapter").count(), 1);
        assert_eq!(output.matches("already installed").count(), 2);
        assert_eq!(output.lines().count(), 3);
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let output = invoke(&context, &["install"]);
        assert!(!output.contains("share one adapter"));
        assert!(!output.contains("Restart"));
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        let status = invoke(&context, &["status"]);
        assert!(status.contains("Pi: found; reporting installed"));
        assert!(status.contains("OMP: found; reporting installed"));
        assert_eq!(status.matches(path.to_str().unwrap()).count(), 2);
        let output = invoke(&context, &["uninstall", "pi"]);
        assert!(!output.contains("share one adapter"));
        assert!(!path.exists());
        let status = invoke(&context, &[]);
        assert!(status.contains("Pi: found; reporting not installed"));
        assert!(status.contains("OMP: found; reporting not installed"));
        invoke(&context, &install);
        let output = invoke(&context, &["uninstall", requested[0], requested[1]]);
        assert_eq!(output.matches("share one adapter").count(), 1);
        assert_eq!(output.matches(": uninstalled").count(), 2);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 0);
    }
}

#[test]
fn separate_extension_files_have_independent_lifecycles_and_no_shared_message() {
    let root = TempDir::new();
    let context = context(&root, &[]);
    for name in ["opencode", "pi", "omp"] {
        fs::create_dir_all(&context.directories[index(name)]).unwrap();
    }
    let output = invoke(&context, &["install", "omp", "pi", "opencode"]);
    assert!(!output.contains("share one adapter"));
    let results = output
        .lines()
        .filter(|line| line.contains(": installed"))
        .collect::<Vec<_>>();
    assert!(results[0].starts_with("OpenCode:"));
    assert!(results[1].starts_with("Pi:"));
    assert!(results[2].starts_with("OMP:"));
    for name in ["opencode", "pi", "omp"] {
        let path = context.file(index(name));
        fs::write(&path, format!("{ADAPTER_MARKER}\n// older copy\n")).unwrap();
        let output = invoke(&context, &["status"]);
        assert!(
            output
                .lines()
                .any(|line| line.contains(path.to_str().unwrap())
                    && line.contains("update available"))
        );
        invoke(&context, &["install", name]);
        let agent = &AGENTS[index(name)];
        let Integration::Extension { adapter, .. } = agent.integration else {
            unreachable!()
        };
        assert_eq!(fs::read_to_string(&path).unwrap(), adapter);
        invoke(&context, &["uninstall", name]);
        assert!(!path.exists());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 0);
    }
}

#[test]
fn extension_table_uses_the_packaged_adapter_sources_and_lists_all_names() {
    assert_eq!(
        AGENTS.iter().map(|agent| agent.name).collect::<Vec<_>>(),
        ["claude", "codex", "gemini", "opencode", "pi", "omp"]
    );
    for (name, expected) in [
        (
            "pi",
            include_str!("../../adapters/pi/delux-agent-status.ts"),
        ),
        (
            "omp",
            include_str!("../../adapters/pi/delux-agent-status.ts"),
        ),
        (
            "opencode",
            include_str!("../../adapters/opencode/delux-agent-status.js"),
        ),
    ] {
        let Integration::Extension { adapter, .. } = AGENTS[index(name)].integration else {
            unreachable!()
        };
        assert_eq!(adapter, expected);
        assert_eq!(adapter.lines().next(), Some(ADAPTER_MARKER));
        for operation in ["install", "uninstall"] {
            let args = ["agents", operation, name].map(str::to_owned);
            assert!(crate::options::parse_options(&args).is_ok());
        }
    }
    let mut help = Vec::new();
    crate::options::print_help(&mut help, "agents");
    assert!(
        String::from_utf8(help)
            .unwrap()
            .contains("claude|codex|gemini|opencode|pi|omp")
    );
}

#[test]
fn omp_profiles_follow_upstream_precedence_and_ignore_named_overrides() {
    let root = TempDir::new();
    let cases: &[(&[(&str, &str)], &str)] = &[
        (&[("OMP_PROFILE", "wkchen")], ".omp/profiles/wkchen/agent"),
        (&[("PI_PROFILE", "legacy")], ".omp/profiles/legacy/agent"),
        (
            &[("OMP_PROFILE", "wkchen"), ("PI_PROFILE", "legacy")],
            ".omp/profiles/wkchen/agent",
        ),
        (
            &[("OMP_PROFILE", ""), ("PI_PROFILE", "legacy")],
            ".omp/agent",
        ),
        (
            &[("OMP_PROFILE", "default"), ("PI_PROFILE", "legacy")],
            ".omp/agent",
        ),
        (
            &[("OMP_PROFILE", "  wkchen  ")],
            ".omp/profiles/wkchen/agent",
        ),
        (
            &[
                ("OMP_PROFILE", "wkchen"),
                ("PI_CODING_AGENT_DIR", "ignored"),
            ],
            ".omp/profiles/wkchen/agent",
        ),
        (
            &[("OMP_PROFILE", "wkchen"), ("PI_CONFIG_DIR", ".custom")],
            ".custom/profiles/wkchen/agent",
        ),
        (
            &[("OMP_PROFILE", "wkchen"), ("PI_CONFIG_DIR", "/.custom")],
            ".custom/profiles/wkchen/agent",
        ),
        (
            &[
                ("OMP_PROFILE", "default"),
                ("PI_CODING_AGENT_DIR", "custom"),
            ],
            "custom",
        ),
        (
            &[
                ("OMP_PROFILE", ""),
                ("PI_PROFILE", "legacy"),
                ("PI_CODING_AGENT_DIR", ".omp/profiles/legacy/agent"),
            ],
            ".omp/agent",
        ),
        (
            &[
                ("OMP_PROFILE", ""),
                ("PI_PROFILE", "legacy"),
                ("PI_CODING_AGENT_DIR", "custom"),
            ],
            "custom",
        ),
        (
            &[
                ("OMP_PROFILE", ""),
                ("PI_PROFILE", "legacy"),
                ("PI_CODING_AGENT_DIR", ".omp/profiles/legacy/./agent"),
            ],
            ".omp/profiles/legacy/agent",
        ),
    ];
    for (variables, expected) in cases {
        let context = context(&root, variables);
        assert_eq!(
            context.directories[index("omp")],
            root.path().join(expected),
            "{variables:?}"
        );
    }
}

#[test]
fn omp_profile_install_status_and_uninstall_use_the_same_named_file() {
    let root = TempDir::new();
    let context = context(
        &root,
        &[
            ("OMP_PROFILE", "wkchen"),
            ("PI_CODING_AGENT_DIR", "ignored"),
        ],
    );
    fs::create_dir_all(&context.directories[index("omp")]).unwrap();
    let file = root
        .path()
        .join(".omp/profiles/wkchen/agent/extensions/delux-agent-status.ts");
    invoke(&context, &["install", "omp"]);
    assert!(file.is_file());
    assert!(
        !root
            .path()
            .join(".omp/agent/extensions/delux-agent-status.ts")
            .exists()
    );
    assert!(
        !root
            .path()
            .join("ignored/extensions/delux-agent-status.ts")
            .exists()
    );
    assert!(invoke(&context, &["status", "omp"]).contains("installed"));
    invoke(&context, &["uninstall", "omp"]);
    assert!(!file.exists());
}

#[test]
fn omp_profiles_reject_invalid_names_and_resolve_relative_default_override() {
    let root = TempDir::new();
    let inputs = |profile: &str, directory: &str| {
        Context::from_inputs(
            Some(root.path().as_os_str().to_owned()),
            OsString::new(),
            root.path().join("delux"),
            root.path().join("definitions"),
            |name| match name {
                "OMP_PROFILE" => Some(profile.into()),
                "PI_CODING_AGENT_DIR" => Some(directory.into()),
                _ => None,
            },
        )
    };
    for profile in [
        "../other",
        ".",
        "..",
        "Name",
        "x.",
        "con",
        "nul.txt",
        "com0",
        "lpt9.log",
        "a/b",
        "a b",
        &"a".repeat(65),
    ] {
        assert!(inputs(profile, "").is_err(), "{profile}");
    }
    let context = inputs("default", "relative/../agent").unwrap();
    assert_eq!(
        context.directories[index("omp")],
        std::env::current_dir().unwrap().join("agent")
    );
}
