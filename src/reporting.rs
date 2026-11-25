//! Versioned codec for the ephemeral, read-only-by-default reporting helper.
use serde::Deserialize;

use crate::tmux::shell_quote;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Reporting,
    Native,
    Missing,
    Installed,
    Outdated,
    Conflict,
    Unknown,
}

impl State {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Reporting => "reporting",
            Self::Native => "native",
            Self::Missing => "missing reporter",
            Self::Installed => "installed · reporting not confirmed",
            Self::Outdated => "update available",
            Self::Conflict => "conflict · not modified",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    Install,
    Update,
}

impl Action {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Install => "Install",
            Self::Update => "Update",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Target {
    pub id: String,
    pub session_id: String,
    pub session_name: String,
    pub pane_id: String,
    pub agent: String,
    pub profile: Option<String>,
    pub state: State,
    pub detail: String,
    pub files: Vec<String>,
    pub token: Option<String>,
    pub action: Option<Action>,
}

impl Target {
    pub(crate) fn actionable(&self) -> bool {
        matches!(
            (self.state, self.action),
            (State::Missing, Some(Action::Install)) | (State::Outdated, Some(Action::Update))
        ) && self.token.as_ref().is_some_and(|token| !token.is_empty())
            && !self.files.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Inspection {
    version: u32,
    pub tmux_version: Option<String>,
    pub native: Option<bool>,
    pub targets: Vec<Target>,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Installed {
    version: u32,
    status: String,
    pub files: Vec<String>,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub(crate) enum Response {
    Inspected(Inspection),
    Installed(Installed),
}

pub(crate) fn command(token: Option<&str>) -> String {
    let payloads = serde_json::json!({
        "pi": include_str!("../adapters/pi/delux-agent-status.ts"),
        "opencode": include_str!("../adapters/opencode/delux-agent-status.js"),
    })
    .to_string();
    let mut command = format!(
        "python3 -c {} {} {}",
        shell_quote(include_str!("../adapters/reporting_setup.py")),
        if token.is_some() {
            "install"
        } else {
            "inspect"
        },
        shell_quote(&payloads)
    );
    if let Some(token) = token {
        command.push(' ');
        command.push_str(&shell_quote(token));
    }
    command
}

fn valid_files(files: &[String]) -> bool {
    files
        .iter()
        .all(|file| file.starts_with('/') && !file.chars().any(char::is_control))
        && files
            .iter()
            .enumerate()
            .all(|(i, file)| !files[..i].contains(file))
}

pub(crate) fn parse(raw: &str, install: bool) -> Result<Response, String> {
    let malformed = || {
        "Reporting check returned an invalid protocol response; recheck before installing."
            .to_owned()
    };
    if install {
        let response: Installed = serde_json::from_str(raw).map_err(|_| malformed())?;
        if response.version != 1
            || response.status != "installed"
            || response.files.is_empty()
            || !valid_files(&response.files)
        {
            return Err(malformed());
        }
        return Ok(Response::Installed(response));
    }
    let response: Inspection = serde_json::from_str(raw).map_err(|_| malformed())?;
    if response.version != 1 {
        return Err(malformed());
    }
    let mut ids = std::collections::HashSet::new();
    for target in &response.targets {
        if target.id.is_empty()
            || !ids.insert(&target.id)
            || target.session_id.is_empty()
            || target.pane_id.is_empty()
            || !matches!(
                target.agent.as_str(),
                "omp" | "pi" | "opencode" | "claude" | "codex" | "gemini" | "unknown"
            )
            || !valid_files(&target.files)
            || (target.action.is_some() && (!target.actionable() || target.agent == "unknown"))
            || (target.action.is_none() && target.token.is_some())
        {
            return Err(malformed());
        }
    }
    Ok(Response::Inspected(response))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspection() -> serde_json::Value {
        serde_json::json!({"version":1,"tmuxVersion":"3.8","native":false,"detail":"Legacy tmux","targets":[{
            "id":"life-1","sessionId":"$0","sessionName":"work","paneId":"%0","agent":"omp","profile":"wkchen",
            "state":"missing","detail":"No adapter","files":["/home/u/.omp/profiles/wkchen/agent/extensions/delux-agent-status.ts"],
            "token":"opaque ' token","action":"install"
        }]})
    }

    #[test]
    fn reporting_protocol_rejects_malformed_and_unsafe_actions() {
        assert!(parse(&inspection().to_string(), false).is_ok());
        for raw in ["", "{}", "[]", "banner\n{}", "{} {}"] {
            assert!(parse(raw, false).is_err());
        }
        for (key, value) in [
            ("state", serde_json::json!("unknown")),
            ("files", serde_json::json!([])),
            ("files", serde_json::json!(["relative"])),
            ("token", serde_json::Value::Null),
            ("action", serde_json::json!("delete")),
        ] {
            let mut data = inspection();
            data["targets"][0][key] = value;
            assert!(parse(&data.to_string(), false).is_err(), "{key}");
        }
        let mut data = inspection();
        data["version"] = 2.into();
        assert!(parse(&data.to_string(), false).is_err());
        let mut data = inspection();
        let duplicate = data["targets"][0].clone();
        data["targets"].as_array_mut().unwrap().push(duplicate);
        assert!(parse(&data.to_string(), false).is_err());
    }

    #[test]
    fn reporting_protocol_install_requires_confirmed_files() {
        assert!(parse(r#"{"version":1,"status":"installed","files":["/tmp/adapter"],"detail":"Restart safely"}"#, true).is_ok());
        for status in ["unknown", "failed", "installed"] {
            let data = serde_json::json!({"version":1,"status":status,"files":[],"detail":""});
            assert!(parse(&data.to_string(), true).is_err());
        }
    }

    #[test]
    fn reporting_command_quotes_opaque_tokens_and_embeds_adapters() {
        let token = "x'; touch /tmp/should-not-run; '";
        let command = command(Some(token));
        assert!(command.ends_with(&shell_quote(token)));
        assert!(command.contains(" install "));
        assert!(command.contains("delux-agent-status adapter"));
        assert!(super::command(None).contains(" inspect "));
    }
}
