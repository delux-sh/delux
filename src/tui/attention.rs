//! Cross-computer attention queue; Browse remains the default.

use super::{AgentState, Picker, Row, RowKind, Selection, Session, Status, Worker};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum View {
    Browse,
    Attention,
}

/// A queue target includes the endpoint and tmux lifetime, not just a reusable ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    worker: String,
    destination: Option<String>,
    session: String,
    created: i64,
    server_pid: i64,
    server_start: i64,
    socket_path: String,
}

impl Identity {
    pub(super) fn new(worker: &Worker, session: &Session) -> Self {
        Self {
            worker: worker.key.clone(),
            destination: worker.destination.clone(),
            session: session.id.clone(),
            created: session.created,
            server_pid: session.server_pid,
            server_start: session.server_start,
            socket_path: session.socket_path.clone(),
        }
    }

    pub(super) fn matches(&self, worker: &Worker, session: &Session) -> bool {
        self.worker == worker.key
            && self.destination == worker.destination
            && self.session == session.id
            && self.created == session.created
            && self.server_pid == session.server_pid
            && self.server_start == session.server_start
            && self.socket_path == session.socket_path
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Coverage {
    pub checked: usize,
    pub checking: usize,
    pub unavailable: usize,
    pub unreported: usize,
    pub discovery_incomplete: bool,
}

impl Picker {
    pub(super) fn switch_view(&mut self) {
        self.view = match self.view {
            View::Browse => View::Attention,
            View::Attention => View::Browse,
        };
        std::mem::swap(&mut self.selection, &mut self.other_selection);
        std::mem::swap(&mut self.filter, &mut self.other_filter);
        self.filtering = false;
        self.cursor = None;
        self.list_state = Default::default();
        self.rebuild();
    }

    pub(super) fn attention_count(&self, state: AgentState) -> usize {
        self.entries
            .iter()
            .flat_map(|entry| entry.sessions())
            .filter(|session| session.agent.is_some_and(|agent| agent.state == state))
            .count()
    }

    pub(super) fn coverage(&self) -> Coverage {
        let mut coverage = Coverage {
            discovery_incomplete: self.scanning || !self.warning.is_empty(),
            ..Coverage::default()
        };
        for entry in &self.entries {
            if self.pending.contains(&entry.worker.key)
                || self.status_pending.contains(&entry.worker.key)
                || (self.scanning && entry.inventory.is_none())
            {
                coverage.checking += 1;
            } else if entry.status() != Status::Ready
                || self.status_errors.contains_key(&entry.worker.key)
            {
                coverage.unavailable += 1;
            } else {
                coverage.checked += 1;
            }
            coverage.unreported += entry
                .sessions()
                .iter()
                .filter(|session| session.agent.is_none())
                .count();
        }
        coverage
    }

    pub(super) fn attention_empty(&self) -> &'static str {
        if !self.filter.is_empty() {
            return "No needs-input sessions match the filter.";
        }
        let coverage = self.coverage();
        if self.scanning {
            "Checking for sessions needing input…"
        } else if self.entries.is_empty() {
            "No computers found; coverage unavailable."
        } else if coverage.discovery_incomplete
            || coverage.unavailable > 0
            || coverage.checking > 0
            || coverage.unreported > 0
        {
            "No reported needs-input sessions; coverage incomplete."
        } else {
            "No sessions currently report needing input."
        }
    }

    pub(super) fn update_attention_order(&mut self) {
        let waiting = self.attention_count(AgentState::Waiting);
        if waiting == 0
            && self.attention_count(AgentState::Error) > 0
            && (self.attention_waiting > 0 || self.attention_order.is_empty())
        {
            self.errors_expanded = true;
        }
        self.attention_waiting = waiting;
        self.attention_order.retain(|identity| {
            self.entries.iter().any(|entry| {
                entry.sessions().iter().any(|session| {
                    identity.matches(&entry.worker, session)
                        && session.agent.is_some_and(|agent| {
                            matches!(agent.state, AgentState::Waiting | AgentState::Error)
                        })
                })
            })
        });
        for entry in &self.entries {
            for session in entry.sessions() {
                if session.agent.is_some_and(|agent| {
                    matches!(agent.state, AgentState::Waiting | AgentState::Error)
                }) && !self
                    .attention_order
                    .iter()
                    .any(|identity| identity.matches(&entry.worker, session))
                {
                    self.attention_order
                        .push(Identity::new(&entry.worker, session));
                }
            }
        }
    }

    /// Appends stable, filtered queue rows, numbering only waiting sessions.
    pub(super) fn append_attention_rows(
        &mut self,
        state: AgentState,
        query: &str,
        limit: usize,
    ) -> usize {
        let matches = |text: &str| caseless::default_case_fold_str(text).contains(query);
        let mut count = 0;
        for identity in &self.attention_order {
            if count == limit {
                break;
            }
            let Some((i, entry)) = self
                .entries
                .iter()
                .enumerate()
                .find(|(_, entry)| entry.worker.key == identity.worker)
            else {
                continue;
            };
            let Some((j, session)) = entry
                .sessions()
                .iter()
                .enumerate()
                .find(|(_, session)| identity.matches(&entry.worker, session))
            else {
                continue;
            };
            if session.agent.is_some_and(|agent| agent.state == state)
                && (query.is_empty()
                    || matches(&session.name)
                    || matches(&entry.worker.name)
                    || entry.worker.destination.as_deref().is_some_and(matches))
            {
                let mut row = Row::new(RowKind::AttentionSession, i, j);
                if state == AgentState::Waiting {
                    if count == 0 {
                        self.rows.push(Row::new(RowKind::NeedsInputHeader, 0, 0));
                    }
                    if count < 9 {
                        row.shortcut = Some(count as u8 + 1);
                    }
                }
                self.rows.push(row);
                count += 1;
            }
        }
        count
    }

    pub(super) fn rebuild_attention(&mut self) {
        let query = caseless::default_case_fold_str(&self.filter);
        let matches = |text: &str| caseless::default_case_fold_str(text).contains(&query);
        self.rows.clear();
        for state in [AgentState::Waiting, AgentState::Error] {
            if state == AgentState::Error {
                if self.attention_count(state) == 0 {
                    continue;
                }
                self.rows.push(Row::new(RowKind::ErrorsHeader, 0, 0));
                if !self.errors_expanded {
                    continue;
                }
            }
            let count = self.append_attention_rows(state, &query, usize::MAX);
            if state == AgentState::Waiting && count == 0 {
                self.rows.push(Row::new(RowKind::NeedsInputHeader, 0, 0));
                self.rows.push(Row::new(RowKind::AttentionEmpty, 0, 0));
            }
        }
        let mut heading = false;
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.status() == Status::Ready
                && !self.status_errors.contains_key(&entry.worker.key)
            {
                continue;
            }
            if !query.is_empty()
                && !matches(&entry.worker.name)
                && !entry.worker.destination.as_deref().is_some_and(matches)
            {
                continue;
            }
            if !heading {
                self.rows.push(Row::new(RowKind::CoverageHeader, 0, 0));
                heading = true;
            }
            if self.checks_expanded {
                self.rows.push(Row::new(RowKind::Computer, i, 0));
            }
        }
        self.cursor = self
            .rows
            .iter()
            .copied()
            .position(|row| match &self.selection {
                Selection::Attention(identity) => self.session(row).is_some_and(|session| {
                    identity.matches(&self.entries[row.entry].worker, session)
                }),
                Selection::Computer(key) => {
                    row.kind == RowKind::Computer && self.entries[row.entry].worker.key == *key
                }
                _ => false,
            });
        if self.filtering && self.cursor.is_none() {
            let retained = match &self.selection {
                Selection::Attention(identity) => self.attention_order.contains(identity),
                Selection::Computer(key) => {
                    self.entries.iter().any(|entry| entry.worker.key == *key)
                }
                Selection::None => true,
                Selection::Session { .. } => false,
            };
            if retained {
                self.cursor = self.rows.iter().copied().position(Self::selectable);
            }
        }
        // Do not silently retarget a removed selection. The connected client and
        // remembered identity survive; explicit navigation chooses a new target.
        if self.selection == Selection::None {
            self.cursor = self.rows.iter().copied().position(Self::selectable);
            self.remember();
        }
    }
}
