//! Program status (OSC 7501): what the programs on this terminal say they are
//! doing — working, blocked on the user, done, failed, idle.
//!
//! The protocol is the third-party "Program Status Protocol", revision 0.4
//! (<https://superlogical.com/rex/docs/build/program-status>); ghost's use of it
//! is in `docs/program-status-design.md`. A program reports
//! `OSC 7501 ; key=value:key=value… ST`, and the terminal keeps one *record* per
//! `id`, each report replacing its record whole. Records belong to the terminal,
//! not a screen buffer: they survive an alternate-screen switch and a soft reset,
//! and only a hard reset drops them. A new shell prompt (OSC 133 `A`) or the
//! program's exit ends the records that describe live work — `working` and
//! `blocked` — while `done` and `error` stay to be seen.
//!
//! Only the capability query (`OSC 7501 ; ?`) is ever answered, and not here:
//! the host's query scanner does it. Records are never echoed back.

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine;

/// What a program says it is doing.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatusState {
    /// At rest, waiting for the user's next instruction.
    Idle,
    /// Running.
    Working,
    /// Finished; the result is ready and not yet seen.
    Done,
    /// Failed and stopped.
    Error,
    /// Cannot go on until the user acts; [`StatusRecord::kind`] says why.
    Blocked,
}

impl StatusState {
    /// How much this state wants the user's attention: the headline is the
    /// record with the highest.
    fn urgency(self) -> u8 {
        match self {
            StatusState::Idle => 0,
            StatusState::Working => 1,
            StatusState::Done => 2,
            StatusState::Error => 3,
            StatusState::Blocked => 4,
        }
    }

    /// Whether this state describes work in flight, which a new shell prompt
    /// or the program's exit ends.
    fn is_live(self) -> bool {
        matches!(self, StatusState::Working | StatusState::Blocked)
    }

    fn name(self) -> &'static str {
        match self {
            StatusState::Idle => "idle",
            StatusState::Working => "working",
            StatusState::Done => "done",
            StatusState::Error => "error",
            StatusState::Blocked => "blocked",
        }
    }
}

/// Why a `blocked` program is waiting.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum BlockedKind {
    /// Approval to act.
    Permission,
    /// An answer the user has to give.
    Question,
    /// A login, token or credential.
    Auth,
}

impl BlockedKind {
    fn name(self) -> &'static str {
        match self {
            BlockedKind::Permission => "permission",
            BlockedKind::Question => "question",
            BlockedKind::Auth => "auth",
        }
    }
}

/// One record: the last report a program made under an `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRecord {
    /// The record's path, `/`-separated; empty for the root record.
    pub id: String,
    pub state: StatusState,
    /// Why it is blocked; only ever set on a `blocked` record.
    pub kind: Option<BlockedKind>,
    /// 0–100 on a `working` or `blocked` record; `None` is indeterminate.
    pub progress: Option<u8>,
    /// The program's stable identifier (`claude-code`), as reported. See
    /// [`ProgramStatus::headline`] for the inherited one.
    pub app: Option<String>,
    /// A short label.
    pub title: Option<String>,
    /// One line of what it is doing, waiting for, or finished.
    pub msg: Option<String>,
}

/// A parsed `OSC 7501` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusReport {
    /// Replace the record at `record.id`.
    Set(StatusRecord),
    /// Remove the record at this id and every record beneath it; the empty id
    /// removes them all.
    Clear(String),
}

/// The most records a terminal keeps; past it the least recently updated goes.
pub const MAX_RECORDS: usize = 256;

/// Longest whole sequence, `OSC` through `ST`.
const MAX_SEQUENCE: usize = 4096;
/// `OSC 7501 ;` and the two-byte `ST` around the pairs.
const FRAMING: usize = 2 + 5 + 2;
const MAX_KEY: usize = 16;
const MAX_APP: usize = 32;
const MAX_ID: usize = 128;
const MAX_ID_SEGMENT: usize = 32;
const MAX_ID_DEPTH: usize = 8;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE: usize = 192;

/// Standard base64, padding optional (the spec allows either).
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Parse the pairs of an `OSC 7501` report (everything after `7501;`). `None`
/// drops the report whole: a broken limit, bad base64, text carrying a control
/// character, a bad id, or a missing or unknown state. A malformed pair or an
/// unknown key is skipped and the rest applied; a repeated key's last value wins.
pub(crate) fn parse(pairs: &str) -> Option<StatusReport> {
    if pairs.len() + FRAMING > MAX_SEQUENCE {
        return None;
    }
    let mut state = None;
    let mut id = None;
    let mut kind = None;
    let mut progress = None;
    let mut app = None;
    let mut title = None;
    let mut msg = None;
    for pair in pairs.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.len() > MAX_KEY {
            return None;
        }
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_lowercase())
            || !value.bytes().all(is_value_byte)
        {
            continue;
        }
        match key {
            "state" => state = Some(value),
            "id" => id = Some(value),
            "kind" => kind = Some(value),
            "progress" => progress = Some(value),
            "app" => app = Some(value),
            "title" => title = Some(value),
            "msg" => msg = Some(value),
            _ => {}
        }
    }

    let id = match id {
        Some(id) => valid_id(id)?.to_string(),
        None => String::new(),
    };
    let state = match state? {
        "clear" => return Some(StatusReport::Clear(id)),
        "idle" => StatusState::Idle,
        "working" => StatusState::Working,
        "done" => StatusState::Done,
        "blocked" => StatusState::Blocked,
        "error" => StatusState::Error,
        _ => return None,
    };
    let app = match app {
        Some(app) if app.len() > MAX_APP => return None,
        Some(app) if !app.is_empty() && app.bytes().all(is_id_byte) => Some(app.to_string()),
        _ => None,
    };
    let title = text(title, MAX_TITLE_ENCODED, MAX_TITLE)?;
    let msg = text(msg, MAX_MSG_ENCODED, MAX_MSG)?;
    let kind = match (state, kind) {
        (StatusState::Blocked, Some("permission")) => Some(BlockedKind::Permission),
        (StatusState::Blocked, Some("question")) => Some(BlockedKind::Question),
        (StatusState::Blocked, Some("auth")) => Some(BlockedKind::Auth),
        _ => None,
    };
    let progress = match state {
        StatusState::Working | StatusState::Blocked => progress
            .and_then(|p| p.parse::<u8>().ok())
            .filter(|p| *p <= 100),
        _ => None,
    };
    Some(StatusReport::Set(StatusRecord {
        id,
        state,
        kind,
        progress,
        app,
        title,
        msg,
    }))
}

/// Encode `record` as the report that recreates it — what a dump replays.
pub(crate) fn encode(record: &StatusRecord) -> String {
    let mut seq = String::from("\x1b]7501;");
    if !record.id.is_empty() {
        seq.push_str("id=");
        seq.push_str(&record.id);
        seq.push(':');
    }
    seq.push_str("state=");
    seq.push_str(record.state.name());
    if let Some(kind) = record.kind {
        seq.push_str(":kind=");
        seq.push_str(kind.name());
    }
    if let Some(progress) = record.progress {
        seq.push_str(&format!(":progress={progress}"));
    }
    if let Some(app) = &record.app {
        seq.push_str(":app=");
        seq.push_str(app);
    }
    if let Some(title) = &record.title {
        seq.push_str(":title=");
        seq.push_str(&BASE64.encode(title));
    }
    if let Some(msg) = &record.msg {
        seq.push_str(":msg=");
        seq.push_str(&BASE64.encode(msg));
    }
    seq.push_str("\x1b\\");
    seq
}

fn is_value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b)
}

fn is_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_.+-".contains(&b)
}

fn valid_id(id: &str) -> Option<&str> {
    let segments: Vec<&str> = id.split('/').collect();
    (id.len() <= MAX_ID
        && segments.len() <= MAX_ID_DEPTH
        && segments
            .iter()
            .all(|s| (1..=MAX_ID_SEGMENT).contains(&s.len()) && s.bytes().all(is_id_byte)))
    .then_some(id)
}

/// Decode a base64 text value. `Some(None)` is an absent (or empty) value;
/// `None` drops the whole report — over a limit, not base64, not UTF-8, or
/// carrying a control character.
fn text(value: Option<&str>, max_encoded: usize, max: usize) -> Option<Option<String>> {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return Some(None);
    };
    if value.len() > max_encoded {
        return None;
    }
    let bytes = BASE64.decode(value).ok()?;
    if bytes.len() > max {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(Some(text))
}

/// Every record a terminal holds, and which one speaks for it.
#[derive(Debug, Clone, Default)]
pub struct ProgramStatus {
    /// Least recently updated first: a report moves its record to the end.
    records: Vec<StatusRecord>,
}

impl PartialEq for ProgramStatus {
    fn eq(&self, other: &Self) -> bool {
        self.records == other.records
    }
}

impl ProgramStatus {
    pub(crate) fn apply(&mut self, report: StatusReport) {
        match report {
            StatusReport::Set(record) => {
                self.records.retain(|r| r.id != record.id);
                if self.records.len() == MAX_RECORDS {
                    self.records.remove(0);
                }
                self.records.push(record);
            }
            StatusReport::Clear(id) if id.is_empty() => self.records.clear(),
            StatusReport::Clear(id) => self.records.retain(|r| !is_within(&r.id, &id)),
        }
    }

    /// The shell drew a new prompt, or the program exited: the work its live
    /// records described is over. `done` and `error` stay to be seen, and so
    /// does `idle` — the spec lets a terminal drop it, but a program at rest
    /// says nothing untrue by staying so.
    pub(crate) fn end_live_work(&mut self) {
        self.records.retain(|r| !r.state.is_live());
    }

    pub(crate) fn reset(&mut self) {
        self.records.clear();
    }

    /// The records, least recently updated first.
    pub fn records(&self) -> &[StatusRecord] {
        &self.records
    }

    /// The record that speaks for the terminal: the most urgent state —
    /// `blocked`, then `error`, `done`, `working`, `idle` — and among equals
    /// the most recently updated. Its `app` is inherited from the nearest
    /// ancestor that has one when it carries none itself. `None` when no
    /// program has reported.
    pub fn headline(&self) -> Option<StatusRecord> {
        // `max_by_key` keeps the last of equal maxima, and the records run
        // oldest first: so the newest of the most urgent.
        let mut headline = self
            .records
            .iter()
            .max_by_key(|r| r.state.urgency())?
            .clone();
        if headline.app.is_none() {
            headline.app = ancestors(&headline.id)
                .find_map(|id| self.records.iter().find(|r| r.id == id)?.app.clone());
        }
        Some(headline)
    }
}

/// Whether `id` is `within` or beneath it.
fn is_within(id: &str, within: &str) -> bool {
    id == within
        || id
            .strip_prefix(within)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `id`'s ancestors, nearest first, ending with the root (`""`).
fn ancestors(id: &str) -> impl Iterator<Item = &str> {
    let mut rest = (!id.is_empty()).then_some(id);
    std::iter::from_fn(move || {
        let current = rest?;
        rest = match current.rfind('/') {
            Some(i) => Some(&current[..i]),
            None if current.is_empty() => None,
            None => Some(""),
        };
        rest
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vt;

    fn b64(s: &str) -> String {
        BASE64.encode(s)
    }

    fn report(vt: &mut Vt, pairs: &str) {
        vt.feed_str(&format!("\x1b]7501;{pairs}\x1b\\"));
    }

    fn headline(vt: &Vt) -> Option<StatusRecord> {
        vt.program_status().headline()
    }

    fn state(vt: &Vt) -> Option<StatusState> {
        headline(vt).map(|r| r.state)
    }

    fn root(state: StatusState) -> StatusRecord {
        StatusRecord {
            id: String::new(),
            state,
            kind: None,
            progress: None,
            app: None,
            title: None,
            msg: None,
        }
    }

    /// What Claude Code 2.1.304 sends at a permission prompt, as captured.
    #[test]
    fn a_claude_code_permission_prompt_reads_back_whole() {
        let mut vt = Vt::new(20, 5);
        let msg = "approve Bash: rm /tmp/x";
        vt.feed_str(&format!(
            "\x1b]7501;state=blocked:app=claude-code:kind=permission:msg={}\x07",
            b64(msg)
        ));
        assert_eq!(
            headline(&vt),
            Some(StatusRecord {
                kind: Some(BlockedKind::Permission),
                app: Some("claude-code".into()),
                msg: Some(msg.into()),
                ..root(StatusState::Blocked)
            })
        );
    }

    #[test]
    fn a_report_replaces_its_record_whole() {
        let mut vt = Vt::new(20, 5);
        report(
            &mut vt,
            &format!("state=working:app=claude-code:msg={}", b64("x")),
        );
        report(&mut vt, "state=done");
        assert_eq!(headline(&vt), Some(root(StatusState::Done)));
        assert_eq!(vt.program_status().records().len(), 1);
    }

    #[test]
    fn whitespace_repeats_and_unknown_or_malformed_pairs_are_tolerated() {
        let mut vt = Vt::new(20, 5);
        report(
            &mut vt,
            " state = idle : future=1 : nonsense : =x : state=working : progress=40 ",
        );
        assert_eq!(
            headline(&vt),
            Some(StatusRecord {
                progress: Some(40),
                ..root(StatusState::Working)
            })
        );
        // A pair with a byte outside the value set is skipped, not fatal.
        report(&mut vt, "state=idle:app=bad app");
        assert_eq!(headline(&vt), Some(root(StatusState::Idle)));
    }

    #[test]
    fn a_report_without_a_known_state_is_dropped() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "state=working");
        report(&mut vt, "app=claude-code");
        report(&mut vt, "state=snoozing");
        assert_eq!(state(&vt), Some(StatusState::Working));
    }

    #[test]
    fn kind_and_progress_count_only_where_they_mean_something() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "state=done:kind=permission:progress=50");
        assert_eq!(headline(&vt), Some(root(StatusState::Done)));
        report(&mut vt, "state=blocked:kind=telepathy:progress=101");
        assert_eq!(headline(&vt), Some(root(StatusState::Blocked)));
    }

    #[test]
    fn a_report_breaking_a_limit_or_carrying_bad_text_is_dropped_whole() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "state=idle");
        let dropped = [
            format!("state=done:msg={}", b64(&"m".repeat(MAX_MSG + 1))),
            format!("state=done:title={}", b64(&"t".repeat(MAX_TITLE + 1))),
            format!("state=done:app={}", "a".repeat(MAX_APP + 1)),
            format!("state=done:{}=1", "k".repeat(MAX_KEY + 1)),
            format!("state=done:msg={}", b64("line\nbreak")),
            format!("state=done:msg={}", b64("bell\u{7}")),
            format!("state=done:title={}", b64("c1\u{85}")),
            // In the value alphabet, but not base64 / not a valid id.
            "state=done:msg=a".into(),
            "state=done:msg=ab=c".into(),
            "state=done:id=a//b".into(),
            "state=done:id=a,b".into(),
            format!("state=done:id={}", "s".repeat(MAX_ID_SEGMENT + 1)),
            format!("state=done:id={}", ["a"; MAX_ID_DEPTH + 1].join("/")),
            format!("state=done:x={}", "v".repeat(MAX_SEQUENCE)),
        ];
        for pairs in dropped {
            report(&mut vt, &pairs);
            assert_eq!(state(&vt), Some(StatusState::Idle), "applied {pairs:?}");
        }
        // Padding is optional, and the longest legal text is kept.
        report(
            &mut vt,
            &format!("state=done:msg={}", b64("ab").trim_end_matches('=')),
        );
        assert_eq!(headline(&vt).unwrap().msg.as_deref(), Some("ab"));
        report(
            &mut vt,
            &format!("state=done:msg={}", b64(&"m".repeat(MAX_MSG))),
        );
        assert_eq!(headline(&vt).unwrap().msg.unwrap().len(), MAX_MSG);
    }

    #[test]
    fn the_most_urgent_record_speaks_and_the_newest_breaks_ties() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "id=a:state=working");
        report(&mut vt, "id=b:state=done");
        assert_eq!(headline(&vt).unwrap().id, "b");
        report(&mut vt, "id=c:state=blocked:kind=question");
        report(&mut vt, "id=d:state=error");
        assert_eq!(headline(&vt).unwrap().id, "c");
        report(&mut vt, "id=e:state=blocked");
        assert_eq!(headline(&vt).unwrap().id, "e");
        report(&mut vt, "id=c:state=blocked:kind=auth");
        assert_eq!(headline(&vt).unwrap().id, "c");
    }

    #[test]
    fn a_record_without_an_app_inherits_its_nearest_ancestors() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "state=idle:app=shell");
        report(&mut vt, "id=build:state=working:app=cargo");
        report(&mut vt, "id=build/test/unit:state=blocked");
        assert_eq!(headline(&vt).unwrap().app.as_deref(), Some("cargo"));
        report(&mut vt, "id=build:state=working");
        assert_eq!(headline(&vt).unwrap().app.as_deref(), Some("shell"));
        // Only the reported record is stored as sent.
        assert!(vt
            .program_status()
            .records()
            .iter()
            .all(|r| r.id.is_empty() || r.app.is_none()));
    }

    #[test]
    fn clear_removes_a_record_and_everything_beneath_it() {
        let mut vt = Vt::new(20, 5);
        for id in ["build", "build/test", "build/test/unit", "builder", "lint"] {
            report(&mut vt, &format!("id={id}:state=working"));
        }
        report(&mut vt, "id=build:state=clear");
        let ids: Vec<_> = vt
            .program_status()
            .records()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(ids, ["builder", "lint"]);
        report(&mut vt, "state=clear");
        assert!(vt.program_status().records().is_empty());
        assert_eq!(headline(&vt), None);
    }

    /// Claude Code's `/exit` sends a bare `clear`, which must leave nothing.
    #[test]
    fn claude_codes_exit_clears_everything() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "state=done:app=claude-code");
        for _ in 0..3 {
            vt.feed_str("\x1b]7501;state=clear\x07");
        }
        assert_eq!(headline(&vt), None);
    }

    #[test]
    fn a_shell_prompt_ends_live_work_but_keeps_results() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "id=a:state=working");
        report(&mut vt, "id=b:state=blocked");
        report(&mut vt, "id=c:state=done");
        report(&mut vt, "id=d:state=error");
        report(&mut vt, "id=e:state=idle");
        vt.feed_str("\x1b]133;A\x07$ ");
        let ids: Vec<_> = vt
            .program_status()
            .records()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(ids, ["c", "d", "e"]);
    }

    #[test]
    fn the_programs_exit_ends_live_work_but_keeps_results() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "id=a:state=blocked");
        report(&mut vt, "id=b:state=done");
        vt.program_exited();
        let ids: Vec<_> = vt
            .program_status()
            .records()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(ids, ["b"]);
    }

    #[test]
    fn records_belong_to_the_terminal_not_the_screen() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "state=working");
        vt.feed_str("\x1b[?1049h");
        report(&mut vt, "id=x:state=done");
        vt.feed_str("\x1b[?1049l\x1b[!p");
        assert_eq!(vt.program_status().records().len(), 2);
        vt.feed_str("\x1bc");
        assert!(vt.program_status().records().is_empty());
    }

    #[test]
    fn past_the_cap_the_least_recently_updated_record_goes() {
        let mut vt = Vt::new(20, 5);
        for i in 0..MAX_RECORDS {
            report(&mut vt, &format!("id=r{i}:state=idle"));
        }
        report(&mut vt, "id=r0:state=working");
        report(&mut vt, "id=new:state=idle");
        let ids: Vec<_> = vt
            .program_status()
            .records()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(ids.len(), MAX_RECORDS);
        assert!(!ids.contains(&"r1".to_string()));
        assert!(ids.contains(&"r0".to_string()) && ids.contains(&"new".to_string()));
    }

    /// The capability query is the host's to answer; it is not a report.
    #[test]
    fn the_query_sets_nothing() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b]7501;?\x1b\\");
        assert!(vt.program_status().records().is_empty());
    }

    /// A reattaching client is re-seeded from the host's dump, so the records
    /// must survive it exactly — order included, which decides ties.
    #[test]
    fn the_dump_replays_every_record_in_order() {
        let mut vt = Vt::new(20, 5);
        report(&mut vt, "state=idle:app=shell");
        report(
            &mut vt,
            &format!(
                "id=build/test:state=blocked:kind=auth:progress=7:title={}:msg={}",
                b64("Tests"),
                b64("log in — ünïcode")
            ),
        );
        report(&mut vt, "id=lint:state=done");
        vt.feed_str("\x1b]133;A\x07$ ");

        let mut replica = Vt::new(20, 5);
        replica.feed_str(&vt.dump());
        assert_eq!(replica.program_status(), vt.program_status());
        assert_eq!(
            replica.program_status().headline(),
            vt.program_status().headline()
        );

        let mut blocked = Vt::new(20, 5);
        report(&mut blocked, "state=blocked:kind=question");
        let mut replica = Vt::new(20, 5);
        replica.feed_str(&blocked.dump());
        assert_eq!(replica.program_status(), blocked.program_status());
    }

    #[test]
    fn ancestors_walk_up_to_the_root() {
        assert_eq!(ancestors("a/b/c").collect::<Vec<_>>(), ["a/b", "a", ""]);
        assert_eq!(ancestors("a").collect::<Vec<_>>(), [""]);
        assert_eq!(ancestors("").count(), 0);
    }
}
