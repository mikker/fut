//! Forwards the client's program-status rollup to the host terminal as OSC 7501.
//!
//! While attached, the client is the host terminal's foreground program, so it
//! owns that terminal's records: a root `app=fut` record summarizing every
//! non-idle Fut terminal, plus one child per terminal keyed by its compact ID.
//! Records are diffed and rate limited, and dropped by a full clear whenever the
//! rollup empties or the client leaves.
use std::{
    collections::BTreeMap,
    io::{self, Write},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{
    domain::{
        BlockedKind, MAX_STATUS_APP_BYTES, MAX_STATUS_MESSAGE_BYTES, MAX_STATUS_TITLE_BYTES,
        StatusState, TerminalId, compact_id,
    },
    resources::ResourceSnapshot,
};

use super::{
    chrome::sanitize,
    config::OuterTerminalStatus,
    federation::{MachineId, State as FederationState},
};

const QUERY: &[u8] = b"\x1b]7501;?\x1b\\";
const CLEAR: &[u8] = b"\x1b]7501;state=clear\x1b\\";
const APP: &str = "fut";
/// Minimum spacing between batches that change state, kind, or text.
const CHANGE_INTERVAL: Duration = Duration::from_millis(100);
/// Minimum spacing between batches that only move progress.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
/// Bounds host output and stays well under the host's per-terminal record cap.
pub(super) const MAX_CHILDREN: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Record {
    pub state: StatusState,
    pub kind: Option<BlockedKind>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
    updated_at_ms: u64,
}

impl Record {
    fn same_except_progress(&self, other: &Self) -> bool {
        self.state == other.state
            && self.kind == other.kind
            && self.app == other.app
            && self.title == other.title
            && self.message == other.message
    }

    fn encode(&self, id: &str, output: &mut Vec<u8>) {
        output.extend_from_slice(b"\x1b]7501;state=");
        output.extend_from_slice(state_name(self.state).as_bytes());
        if !id.is_empty() {
            output.extend_from_slice(b":id=");
            output.extend_from_slice(id.as_bytes());
        }
        if let Some(kind) = self.kind {
            output.extend_from_slice(b":kind=");
            output.extend_from_slice(kind_name(kind).as_bytes());
        }
        if let Some(progress) = self.progress {
            output.extend_from_slice(format!(":progress={progress}").as_bytes());
        }
        if let Some(app) = &self.app {
            output.extend_from_slice(b":app=");
            output.extend_from_slice(app.as_bytes());
        }
        for (key, text) in [("title", &self.title), ("msg", &self.message)] {
            if let Some(text) = text {
                output.extend_from_slice(format!(":{key}=").as_bytes());
                output.extend_from_slice(STANDARD.encode(text).as_bytes());
            }
        }
        output.extend_from_slice(b"\x1b\\");
    }
}

/// The derived record set. The root uses the empty ID.
pub(super) type Records = BTreeMap<String, Record>;

/// One machine's resources as seen by this client.
pub(super) struct Source<'a> {
    pub label: Option<&'a str>,
    pub snapshot: &'a ResourceSnapshot,
}

/// Every machine this client shows: the live attachment, plus current
/// federated endpoints when more than one machine is registered.
pub(super) fn sources<'a>(
    live: Option<&'a ResourceSnapshot>,
    federation: Option<&'a FederationState>,
    active: MachineId,
) -> Vec<Source<'a>> {
    let federated = federation.filter(|state| state.registry.iter().count() > 1);
    let Some(state) = federated else {
        return live
            .map(|snapshot| Source {
                label: None,
                snapshot,
            })
            .into_iter()
            .collect();
    };
    let mut sources = Vec::new();
    for (machine, endpoint) in state.registry.iter() {
        let label = Some(endpoint.spec.label());
        if machine == active {
            if let Some(snapshot) = live {
                sources.push(Source { label, snapshot });
            }
        } else if let Some(resources) = endpoint.metadata.resources.as_ref()
            && endpoint.is_current(resources)
        {
            sources.push(Source {
                label,
                snapshot: &resources.value,
            });
        }
    }
    sources
}

fn priority(state: StatusState) -> u8 {
    // Mirrors ProgramStatus::summary_record.
    match state {
        StatusState::Blocked => 4,
        StatusState::Error => 3,
        StatusState::Working => 2,
        StatusState::Done => 1,
        StatusState::Idle => 0,
    }
}

/// Derive the host records from every visible machine. `own_terminal` is the
/// Fut terminal hosting this client, if any; it is skipped so a client attached
/// to its own daemon cannot feed its rollup back into itself.
pub(super) fn derive(sources: &[Source<'_>], own_terminal: Option<TerminalId>) -> Records {
    let mut children: Vec<(String, Record)> = Vec::new();
    for source in sources {
        for path in source.snapshot.pane_paths() {
            if path.session.closing
                || path.workspace.closing
                || path.tab.closing
                || path.pane.closing
                || Some(path.pane.terminal_id) == own_terminal
            {
                continue;
            }
            let activity = &path.pane.activity;
            let Some(summary) = activity.summary_record() else {
                continue;
            };
            if summary.state == StatusState::Idle {
                continue;
            }
            let id = compact_id(path.pane.terminal_id.uuid());
            if children.iter().any(|(existing, _)| *existing == id) {
                continue;
            }
            let location = source
                .label
                .into_iter()
                .chain([
                    path.session.name.as_str(),
                    path.workspace.name.as_str(),
                    path.tab.name.as_str(),
                ])
                .collect::<Vec<_>>()
                .join("/");
            let detail = [summary.title.as_deref(), summary.message.as_deref()]
                .into_iter()
                .flatten()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            children.push((
                id,
                Record {
                    state: summary.state,
                    kind: summary.kind,
                    progress: activity.progress(),
                    app: activity
                        .app(summary)
                        .or(activity.label())
                        .filter(|app| is_name(app))
                        .map(str::to_owned),
                    title: text(&location, MAX_STATUS_TITLE_BYTES),
                    message: text(&detail, MAX_STATUS_MESSAGE_BYTES),
                    updated_at_ms: activity.updated_at_ms(),
                },
            ));
        }
    }
    if children.is_empty() {
        return Records::new();
    }
    children.sort_by(|(left_id, left), (right_id, right)| {
        (priority(right.state), right.updated_at_ms)
            .cmp(&(priority(left.state), left.updated_at_ms))
            .then_with(|| left_id.cmp(right_id))
    });
    children.truncate(MAX_CHILDREN);
    let (_, summary) = &children[0];
    let mut active = children
        .iter()
        .filter(|(_, record)| matches!(record.state, StatusState::Working | StatusState::Blocked));
    let progress = match (active.next(), active.next()) {
        (Some((_, record)), None) => record.progress,
        _ => None,
    };
    let message = match (&summary.title, &summary.message) {
        (Some(title), Some(message)) => format!("{title}: {message}"),
        (Some(text), None) | (None, Some(text)) => text.clone(),
        (None, None) => String::new(),
    };
    let root = Record {
        state: summary.state,
        kind: summary.kind,
        progress,
        app: Some(APP.into()),
        title: None,
        message: text(&message, MAX_STATUS_MESSAGE_BYTES),
        updated_at_ms: summary.updated_at_ms,
    };
    let mut records: Records = children.into_iter().collect();
    records.insert(String::new(), root);
    records
}

fn is_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_STATUS_APP_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
}

/// Plain single-line text within `limit` bytes, or `None` when empty.
fn text(value: &str, limit: usize) -> Option<String> {
    let mut value = sanitize(value);
    if value.len() > limit {
        let mut end = limit - '…'.len_utf8();
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push('…');
    }
    (!value.is_empty()).then_some(value)
}

fn state_name(state: StatusState) -> &'static str {
    match state {
        StatusState::Idle => "idle",
        StatusState::Working => "working",
        StatusState::Done => "done",
        StatusState::Blocked => "blocked",
        StatusState::Error => "error",
    }
}

fn kind_name(kind: BlockedKind) -> &'static str {
    match kind {
        BlockedKind::Permission => "permission",
        BlockedKind::Question => "question",
        BlockedKind::Auth => "auth",
    }
}

/// Encode the bytes that move the host from `emitted` to `desired`.
pub(super) fn encode_diff(emitted: &Records, desired: &Records) -> Vec<u8> {
    let mut output = Vec::new();
    if desired.is_empty() {
        if !emitted.is_empty() {
            output.extend_from_slice(CLEAR);
        }
        return output;
    }
    if emitted.is_empty() {
        // Claim the host terminal, sweeping records a killed client left behind.
        output.extend_from_slice(CLEAR);
    }
    for id in emitted.keys().filter(|id| !desired.contains_key(*id)) {
        output.extend_from_slice(b"\x1b]7501;state=clear:id=");
        output.extend_from_slice(id.as_bytes());
        output.extend_from_slice(b"\x1b\\");
    }
    // Children before the root so the host sees a coherent tree.
    for (id, record) in desired.iter().filter(|(id, _)| !id.is_empty()) {
        if emitted.get(id) != Some(record) {
            record.encode(id, &mut output);
        }
    }
    if let Some(root) = desired.get("")
        && emitted.get("") != Some(root)
    {
        root.encode("", &mut output);
    }
    output
}

pub(super) struct HostStatus {
    mode: OuterTerminalStatus,
    queried: bool,
    supported: bool,
    own_terminal: Option<TerminalId>,
    desired: Records,
    emitted: Records,
    last_batch: Option<Instant>,
}

impl HostStatus {
    pub(super) fn new(own_terminal: Option<TerminalId>) -> Self {
        Self {
            mode: OuterTerminalStatus::Never,
            queried: false,
            supported: false,
            own_terminal,
            desired: Records::new(),
            emitted: Records::new(),
            last_batch: None,
        }
    }

    pub(super) fn own_terminal_from_env() -> Option<TerminalId> {
        std::env::var("FUT_TERMINAL_ID").ok()?.parse().ok()
    }

    /// Apply the configured mode, probing the host the first time it is needed.
    pub(super) fn configure(
        &mut self,
        mode: OuterTerminalStatus,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        self.mode = mode;
        if mode == OuterTerminalStatus::Auto && !self.queried {
            self.queried = true;
            writer.write_all(QUERY)?;
            writer.flush()?;
        }
        Ok(())
    }

    /// Note a host reply. Returns whether it was the OSC 7501 capability reply.
    pub(super) fn receive(&mut self, response: &[u8]) -> bool {
        let reply = response.starts_with(b"\x1b]7501;?");
        if reply {
            self.supported = true;
        }
        reply
    }

    pub(super) fn enabled(&self) -> bool {
        match self.mode {
            OuterTerminalStatus::Always => true,
            OuterTerminalStatus::Auto => self.supported,
            OuterTerminalStatus::Never => false,
        }
    }

    pub(super) fn observe(&mut self, sources: &[Source<'_>]) {
        self.desired = if self.enabled() {
            derive(sources, self.own_terminal)
        } else {
            Records::new()
        };
    }

    fn interval(&self) -> Option<Duration> {
        if self.desired == self.emitted {
            return None;
        }
        let structural = self.desired.len() != self.emitted.len()
            || self.desired.iter().any(|(id, record)| {
                self.emitted
                    .get(id)
                    .is_none_or(|emitted| !emitted.same_except_progress(record))
            });
        Some(if structural {
            CHANGE_INTERVAL
        } else {
            PROGRESS_INTERVAL
        })
    }

    /// When the next coalesced batch is due, if one is pending.
    pub(super) fn deadline(&self) -> Option<Instant> {
        let interval = self.interval()?;
        Some(
            self.last_batch
                .map_or_else(Instant::now, |last| last + interval),
        )
    }

    /// The next batch to write, if one is pending and due.
    pub(super) fn poll(&mut self, now: Instant) -> Option<Vec<u8>> {
        let interval = self.interval()?;
        if self.last_batch.is_some_and(|last| now < last + interval) {
            return None;
        }
        let output = encode_diff(&self.emitted, &self.desired);
        self.emitted = self.desired.clone();
        self.last_batch = Some(now);
        (!output.is_empty()).then_some(output)
    }

    /// Write any due batch to the host.
    pub(super) fn flush(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if let Some(output) = self.poll(Instant::now()) {
            writer.write_all(&output)?;
            writer.flush()?;
        }
        Ok(())
    }

    /// Release the host terminal's records. Safe to call repeatedly.
    pub(super) fn release(&mut self, writer: &mut impl Write) -> io::Result<()> {
        self.desired.clear();
        if !self.emitted.is_empty() {
            self.emitted.clear();
            writer.write_all(CLEAR)?;
            writer.flush()?;
        }
        Ok(())
    }
}

impl Drop for HostStatus {
    fn drop(&mut self) {
        let _ = self.release(&mut io::stdout());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        domain::{
            PaneId, ProgramStatus, SessionId, StatusRecord, StatusSource, TabId, WorkspaceId,
        },
        resources::{
            PaneSnapshot, Project, ProjectIdentity, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
        },
        splits::SplitTree,
    };

    fn record(id: &str, state: StatusState, revision: u64) -> StatusRecord {
        StatusRecord {
            id: id.into(),
            state,
            kind: None,
            progress: None,
            app: None,
            title: None,
            message: None,
            revision,
            updated_at_ms: revision,
            attention_revision: 0,
        }
    }

    fn pane(records: Vec<StatusRecord>) -> PaneSnapshot {
        PaneSnapshot {
            name: None,
            tokens: Default::default(),
            id: PaneId::new(),
            terminal_id: TerminalId::new(),
            closing: false,
            activity: ProgramStatus {
                records,
                source: Some(StatusSource::Osc),
                revision: 1,
                attention_revision: 0,
                read_revision: 0,
                working_revision: 0,
            },
            cwd: None,
            worktree: None,
        }
    }

    fn resources_with(panes: Vec<PaneSnapshot>) -> ResourceSnapshot {
        let tabs = panes
            .into_iter()
            .enumerate()
            .map(|(index, pane)| TabSnapshot {
                tokens: Default::default(),
                id: TabId::new(),
                name: format!("tab{index}"),
                closing: false,
                layout: SplitTree::leaf(pane.id),
                panes: vec![pane],
            })
            .collect();
        ResourceSnapshot {
            revision: 1,
            sessions: vec![SessionSnapshot {
                tokens: Default::default(),
                id: SessionId::new(),
                name: "proj".into(),
                project: Project {
                    identity: ProjectIdentity::CanonicalDirectory(PathBuf::from("/proj")),
                },
                trusted_project_config: None,
                closing: false,
                workspaces: vec![WorkspaceSnapshot {
                    tokens: Default::default(),
                    id: WorkspaceId::new(),
                    parent_workspace_id: None,
                    name: "main".into(),
                    root: PathBuf::from("/proj"),
                    closing: false,
                    tabs,
                }],
            }],
        }
    }

    fn local(snapshot: &ResourceSnapshot) -> Vec<Source<'_>> {
        vec![Source {
            label: None,
            snapshot,
        }]
    }

    fn terminal(snapshot: &ResourceSnapshot, index: usize) -> TerminalId {
        snapshot.sessions[0].workspaces[0].tabs[index].panes[0].terminal_id
    }

    #[test]
    fn derives_a_fut_root_and_one_child_per_non_idle_terminal() {
        let mut blocked = record("tool", StatusState::Blocked, 5);
        blocked.kind = Some(BlockedKind::Permission);
        blocked.title = Some("Approve".into());
        blocked.message = Some("rm\u{202e} -rf".into());
        let mut agent_root = record("", StatusState::Working, 4);
        agent_root.app = Some("claude-code".into());
        let mut build = record("", StatusState::Working, 3);
        build.progress = Some(40);
        build.app = Some("bad app".into());
        let snapshot = resources_with(vec![
            pane(vec![agent_root, blocked]),
            pane(vec![build]),
            pane(vec![record("", StatusState::Idle, 9)]),
            pane(Vec::new()),
        ]);
        let records = derive(&local(&snapshot), None);
        assert_eq!(records.len(), 3, "root plus two non-idle terminals");

        let agent = &records[&compact_id(terminal(&snapshot, 0).uuid())];
        assert_eq!(agent.state, StatusState::Blocked);
        assert_eq!(agent.kind, Some(BlockedKind::Permission));
        assert_eq!(agent.app.as_deref(), Some("claude-code"), "inherited app");
        assert_eq!(agent.title.as_deref(), Some("proj/main/tab0"));
        assert_eq!(agent.message.as_deref(), Some("Approve · rm� -rf"));

        let build = &records[&compact_id(terminal(&snapshot, 1).uuid())];
        assert_eq!(build.progress, Some(40));
        assert_eq!(build.app, None, "invalid names inherit fut");

        let root = &records[""];
        assert_eq!(root.app.as_deref(), Some("fut"));
        assert_eq!(root.state, StatusState::Blocked, "blocked wins");
        assert_eq!(root.kind, Some(BlockedKind::Permission));
        assert_eq!(root.progress, None, "two active terminals");
        assert_eq!(
            root.message.as_deref(),
            Some("proj/main/tab0: Approve · rm� -rf")
        );
        for id in records.keys().filter(|id| !id.is_empty()) {
            assert!(id.len() <= 32 && is_name(id), "valid id segment {id}");
        }
    }

    #[test]
    fn root_follows_precedence_and_single_active_progress() {
        let mut working = record("", StatusState::Working, 1);
        working.progress = Some(70);
        let snapshot = resources_with(vec![
            pane(vec![working]),
            pane(vec![record("", StatusState::Done, 2)]),
        ]);
        let records = derive(&local(&snapshot), None);
        assert_eq!(records[""].state, StatusState::Working);
        assert_eq!(records[""].progress, Some(70));

        let errored = resources_with(vec![
            pane(vec![record("", StatusState::Done, 3)]),
            pane(vec![record("", StatusState::Error, 1)]),
        ]);
        assert_eq!(derive(&local(&errored), None)[""].state, StatusState::Error);
        let done = resources_with(vec![pane(vec![record("", StatusState::Done, 3)])]);
        assert_eq!(derive(&local(&done), None)[""].state, StatusState::Done);
    }

    #[test]
    fn legacy_progress_children_inherit_the_fut_app() {
        let mut progress = record("", StatusState::Working, 1);
        progress.progress = Some(40);
        let mut legacy = pane(vec![progress]);
        legacy.activity.source = Some(StatusSource::legacy_progress());
        let snapshot = resources_with(vec![legacy]);
        let records = derive(&local(&snapshot), None);
        let child = &records[&compact_id(terminal(&snapshot, 0).uuid())];
        assert_eq!(child.progress, Some(40));
        assert_eq!(child.app, None, "OSC 9;4 does not name a program");
    }

    #[test]
    fn skips_its_own_terminal_closing_panes_and_caps_children() {
        let mut panes: Vec<_> = (0..MAX_CHILDREN + 5)
            .map(|index| pane(vec![record("", StatusState::Working, index as u64)]))
            .collect();
        panes[1].closing = true;
        let snapshot = resources_with(panes);
        let own = terminal(&snapshot, 0);
        let records = derive(&local(&snapshot), Some(own));
        assert_eq!(records.len(), MAX_CHILDREN + 1);
        assert!(!records.contains_key(&compact_id(own.uuid())));
        assert!(!records.contains_key(&compact_id(terminal(&snapshot, 1).uuid())));
    }

    #[test]
    fn federated_machines_prefix_their_label() {
        let remote = resources_with(vec![pane(vec![record("", StatusState::Working, 1)])]);
        let records = derive(
            &[Source {
                label: Some("box"),
                snapshot: &remote,
            }],
            None,
        );
        let child = &records[&compact_id(terminal(&remote, 0).uuid())];
        assert_eq!(child.title.as_deref(), Some("box/proj/main/tab0"));
    }

    #[test]
    fn long_text_is_truncated_on_a_character_boundary() {
        let value = "é".repeat(MAX_STATUS_MESSAGE_BYTES);
        let truncated = text(&value, MAX_STATUS_MESSAGE_BYTES).unwrap();
        assert!(truncated.len() <= MAX_STATUS_MESSAGE_BYTES);
        assert!(truncated.ends_with('…'));
        assert_eq!(text("", 10), None);
    }

    #[test]
    fn diffs_claim_update_remove_and_clear_instead_of_done() {
        let snapshot = resources_with(vec![
            pane(vec![record("", StatusState::Working, 1)]),
            pane(vec![record("", StatusState::Working, 2)]),
        ]);
        let first = derive(&local(&snapshot), None);
        let child = |index| compact_id(terminal(&snapshot, index).uuid());
        let claim = String::from_utf8(encode_diff(&Records::new(), &first)).unwrap();
        assert!(claim.starts_with("\x1b]7501;state=clear\x1b\\"));
        assert!(claim.contains(&format!("\x1b]7501;state=working:id={}:title=", child(0))));
        assert!(claim.ends_with(&format!(
            "\x1b]7501;state=working:app=fut:msg={}\x1b\\",
            STANDARD.encode("proj/main/tab1")
        )));
        assert!(encode_diff(&first, &first).is_empty());

        let mut second = first.clone();
        second.remove(&child(0));
        let update = String::from_utf8(encode_diff(&first, &second)).unwrap();
        assert_eq!(
            update,
            format!("\x1b]7501;state=clear:id={}\x1b\\", child(0))
        );
        assert_eq!(encode_diff(&second, &Records::new()), CLEAR);
        assert!(encode_diff(&Records::new(), &Records::new()).is_empty());
    }

    #[test]
    fn every_sequence_fits_the_protocol_bounds() {
        let mut long = record("", StatusState::Blocked, 1);
        long.kind = Some(BlockedKind::Question);
        long.progress = Some(100);
        long.app = Some("a".repeat(MAX_STATUS_APP_BYTES));
        long.title = Some("t".repeat(MAX_STATUS_TITLE_BYTES));
        long.message = Some("m".repeat(MAX_STATUS_MESSAGE_BYTES));
        let mut snapshot = resources_with(vec![pane(vec![long])]);
        snapshot.sessions[0].name = "s".repeat(500);
        let records = derive(&local(&snapshot), None);
        let output = encode_diff(&Records::new(), &records);
        for sequence in output.split_inclusive(|byte| *byte == b'\\') {
            assert!(sequence.len() <= 4096, "{} bytes", sequence.len());
        }
    }

    #[test]
    fn coalesces_changes_and_progress_and_releases_once() {
        let mut host = HostStatus::new(None);
        host.configure(OuterTerminalStatus::Auto, &mut Vec::new())
            .unwrap();
        let mut working = record("", StatusState::Working, 1);
        working.progress = Some(10);
        let first = resources_with(vec![pane(vec![working.clone()])]);
        host.observe(&local(&first));
        assert_eq!(host.deadline(), None, "nothing before a capability reply");
        assert!(!host.receive(b"\x1b]11;rgb:0/0/0\x07"));
        assert!(host.receive(b"\x1b]7501;?:states=idle\x1b\\"));
        host.observe(&local(&first));

        let start = Instant::now();
        assert!(host.poll(start).is_some(), "leading edge emits at once");
        assert_eq!(host.poll(start), None, "unchanged sets send nothing");

        let mut progressed = first.clone();
        progressed.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .records[0]
            .progress = Some(20);
        host.observe(&local(&progressed));
        assert_eq!(host.poll(start + CHANGE_INTERVAL), None, "progress waits");
        assert_eq!(host.deadline(), Some(start + PROGRESS_INTERVAL));
        let batch = host.poll(start + PROGRESS_INTERVAL).unwrap();
        assert!(String::from_utf8(batch).unwrap().contains(":progress=20"));

        let later = start + PROGRESS_INTERVAL;
        let mut blocked = progressed.clone();
        blocked.sessions[0].workspaces[0].tabs[0].panes[0]
            .activity
            .records[0]
            .state = StatusState::Blocked;
        host.observe(&local(&blocked));
        assert_eq!(host.poll(later + CHANGE_INTERVAL / 2), None);
        assert!(host.poll(later + CHANGE_INTERVAL).is_some());

        let empty = resources_with(Vec::new());
        host.observe(&local(&empty));
        assert_eq!(host.poll(later + CHANGE_INTERVAL * 2).unwrap(), CLEAR);

        host.observe(&local(&first));
        host.poll(later + CHANGE_INTERVAL * 3).unwrap();
        let mut released = Vec::new();
        host.release(&mut released).unwrap();
        assert_eq!(released, CLEAR);
        released.clear();
        host.release(&mut released).unwrap();
        assert!(released.is_empty(), "release is idempotent");
    }

    #[test]
    fn modes_control_probing_and_emission() {
        let snapshot = resources_with(vec![pane(vec![record("", StatusState::Working, 1)])]);

        let mut always = HostStatus::new(None);
        let mut probe = Vec::new();
        always
            .configure(OuterTerminalStatus::Always, &mut probe)
            .unwrap();
        assert!(probe.is_empty(), "always does not probe");
        always.observe(&local(&snapshot));
        assert!(always.poll(Instant::now()).is_some());
        always.emitted.clear();

        let mut auto = HostStatus::new(None);
        auto.configure(OuterTerminalStatus::Auto, &mut probe)
            .unwrap();
        auto.configure(OuterTerminalStatus::Auto, &mut probe)
            .unwrap();
        assert_eq!(probe, QUERY, "probes once");

        auto.receive(b"\x1b]7501;?\x07");
        auto.observe(&local(&snapshot));
        assert!(auto.poll(Instant::now()).is_some());
        auto.configure(OuterTerminalStatus::Never, &mut probe)
            .unwrap();
        auto.observe(&local(&snapshot));
        let now = Instant::now() + CHANGE_INTERVAL;
        assert_eq!(auto.poll(now).unwrap(), CLEAR, "disabling releases");
        auto.emitted.clear();
    }
}
