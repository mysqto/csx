//! Background daemon logic: debounced re-indexing and a query request handler.
//!
//! This module holds only decision logic over ports, so it is fully unit-tested
//! with fakes:
//!
//! * a [`Clock`] the debouncer reads instead of the wall clock,
//! * a [`Debouncer`] that coalesces a burst of change events into one flush
//!   once the watched set has been quiet for a window,
//! * [`watch_loop`], which pumps a [`FileWatcher`] through the debouncer and
//!   calls the incremental indexer on each flush, and
//! * [`handle_request`] / [`serve_once`], which parse one query request line,
//!   run [`Db::search`], and serialize a response back over a [`Conn`].
//!
//! The real filesystem watcher and socket transport live in `watch_shim.rs`
//! and `transport_shim.rs`; nothing here touches the OS or the network.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db::{Db, Scope, SearchOpts};
use crate::error::Result;
use crate::git_shim::GitRunner;
use crate::index::{sync, SyncStats};
use crate::source::SessionSource;
use crate::transport_shim::{Conn, Listener};
use crate::watch_shim::FileWatcher;

/// Port over a monotonic time source, so the debounce window is testable.
///
/// The real daemon uses [`crate::clock_shim::SystemClock`]; tests use a clock
/// they advance by hand.
pub trait Clock {
    /// Milliseconds elapsed since some fixed, monotonic epoch.
    fn now_ms(&self) -> u64;
}

/// Coalesces a burst of change events into a single flush.
///
/// Each observed event marks the debouncer dirty and records the time. A flush
/// becomes due only once the watched set has been quiet — no further event — for
/// `window_ms`. This turns an editor's flurry of writes (or a tool appending
/// many lines to a transcript) into one incremental re-index.
#[derive(Debug)]
pub struct Debouncer {
    window_ms: u64,
    dirty: bool,
    last_event_ms: u64,
}

impl Debouncer {
    /// Create a debouncer with a quiet-window of `window_ms` milliseconds.
    pub fn new(window_ms: u64) -> Self {
        Debouncer {
            window_ms,
            dirty: false,
            last_event_ms: 0,
        }
    }

    /// Whether at least one event is pending a flush.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Record that a change happened at `now_ms`.
    pub fn record(&mut self, now_ms: u64) {
        self.dirty = true;
        self.last_event_ms = now_ms;
    }

    /// If dirty and the quiet window has elapsed by `now_ms`, clear the dirty
    /// flag and return `true` (a flush is due); otherwise return `false`.
    pub fn take_due(&mut self, now_ms: u64) -> bool {
        if self.dirty && now_ms.saturating_sub(self.last_event_ms) >= self.window_ms {
            self.dirty = false;
            true
        } else {
            false
        }
    }
}

/// How long the watch loop blocks in a single [`FileWatcher::poll`] call. Kept
/// small so the debounce window is checked promptly even without new events.
const POLL_TIMEOUT: Duration = Duration::from_millis(200);

/// Drive one iteration of the watch loop: poll once, fold any event into the
/// debouncer, then flush (incremental re-index) if the quiet window elapsed.
///
/// Returns `Ok(Some(stats))` when a flush ran, `Ok(None)` when nothing was due,
/// and propagates a watcher/index error. Split out from [`watch_loop`] so the
/// full poll → debounce → index path is unit-testable with a fake watcher and
/// a hand-advanced clock.
pub fn watch_step(
    watcher: &mut dyn FileWatcher,
    debouncer: &mut Debouncer,
    clock: &dyn Clock,
    sources: &[Box<dyn SessionSource>],
    db: &Db,
    git: &dyn GitRunner,
) -> Result<Option<SyncStats>> {
    if let Some(_event) = watcher.poll(POLL_TIMEOUT)? {
        debouncer.record(clock.now_ms());
    }
    if debouncer.take_due(clock.now_ms()) {
        let stats = sync(sources, db, git)?;
        Ok(Some(stats))
    } else {
        Ok(None)
    }
}

/// Run the watch loop until the watcher errors (e.g. the backend disconnects).
///
/// Each step polls for a change and re-indexes once a burst settles. Any pending
/// dirty state is flushed on the way out so a final burst is never dropped.
pub fn watch_loop(
    watcher: &mut dyn FileWatcher,
    debouncer: &mut Debouncer,
    clock: &dyn Clock,
    sources: &[Box<dyn SessionSource>],
    db: &Db,
    git: &dyn GitRunner,
) -> Result<()> {
    loop {
        match watch_step(watcher, debouncer, clock, sources, db, git) {
            Ok(_) => {}
            Err(e) => {
                if debouncer.is_dirty() {
                    let _ = sync(sources, db, git);
                }
                return Err(e);
            }
        }
    }
}

/// A single scope filter carried in a query request. Mirrors the optional
/// fields of a storage [`Scope`]; every field defaults to absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestScope {
    /// Restrict to a source account UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    /// Restrict to a source org UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    /// Restrict to a source profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Restrict to a tool identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Restrict to a repository id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    /// Restrict to a session project path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Restrict to a git branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Restrict to a single session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Restrict to a message role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Restrict to a message tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Lower bound on message timestamp (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    /// Upper bound on message timestamp (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

impl RequestScope {
    /// Lower this request scope into a storage [`Scope`].
    pub fn to_scope(&self) -> Scope {
        Scope {
            account_uuid: self.account_uuid.clone(),
            org_uuid: self.org_uuid.clone(),
            profile: self.profile.clone(),
            tool: self.tool.clone(),
            repo_id: self.repo_id.clone(),
            cwd: self.cwd.clone(),
            branch: self.branch.clone(),
            session_id: self.session_id.clone(),
            role: self.role.clone(),
            tool_name: self.tool_name.clone(),
            since: self.since,
            until: self.until,
        }
    }
}

/// Default hit limit when a request omits `limit`.
fn default_limit() -> usize {
    20
}

/// A query request line: the text to search, an optional scope, and tuning.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct QueryRequest {
    /// The query text (FTS, or trigram substring when `code` is set).
    pub query: String,
    /// Scope filters, all optional and ANDed together.
    #[serde(default)]
    pub scope: RequestScope,
    /// Use the trigram (substring / code) index.
    #[serde(default)]
    pub code: bool,
    /// Maximum number of hits to return.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// One hit in a query response, a flat serializable projection of [`crate::db::Hit`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseHit {
    /// Session the matching message belongs to.
    pub session_id: String,
    /// Tool that produced the message.
    pub tool: Option<String>,
    /// Repository id of the session, when known.
    pub repo_id: Option<String>,
    /// Human-friendly project name of the session, when known.
    pub project_name: Option<String>,
    /// Message timestamp (unix seconds).
    pub ts: i64,
    /// Highlighted excerpt from the matching body.
    pub snippet: String,
    /// Blended relevance score (higher is better).
    pub score: f64,
}

/// A query response: either the ranked hits, or an error string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum QueryResponse {
    /// A successful search, carrying the ranked hits.
    Ok {
        /// The ranked hits, best first.
        hits: Vec<ResponseHit>,
    },
    /// A failed request, carrying a human-readable message.
    Error {
        /// The failure reason (bad request, or a search error).
        message: String,
    },
}

impl QueryResponse {
    /// Serialize to a single newline-terminated response line.
    pub fn to_line(&self) -> String {
        // Serialization of this closed enum cannot fail; fall back defensively.
        let body = serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"status":"error","message":"serialize failed"}"#.to_string());
        format!("{body}\n")
    }
}

/// Turn a parsed request into a response by running the search against `db`.
///
/// Never returns `Err`: a search failure is folded into a [`QueryResponse::Error`]
/// so the caller can always write one response line and move on.
pub fn handle_request(db: &Db, req: &QueryRequest) -> QueryResponse {
    let scope = req.scope.to_scope();
    let opts = SearchOpts {
        code: req.code,
        limit: req.limit,
    };
    match db.search(&req.query, &scope, &opts) {
        Ok(hits) => QueryResponse::Ok {
            hits: hits.iter().map(hit_to_response).collect(),
        },
        Err(e) => QueryResponse::Error {
            message: e.to_string(),
        },
    }
}

/// Project a storage [`crate::db::Hit`] into a serializable [`ResponseHit`].
fn hit_to_response(h: &crate::db::Hit) -> ResponseHit {
    ResponseHit {
        session_id: h.session_id.clone(),
        tool: h.tool.clone(),
        repo_id: h.repo_id.clone(),
        project_name: h.project_name.clone(),
        ts: h.ts,
        snippet: h.snippet.clone(),
        score: h.score,
    }
}

/// Parse one request line into a [`QueryRequest`], mapping a JSON error into a
/// [`QueryResponse::Error`] describing the bad request.
pub fn parse_request(line: &str) -> std::result::Result<QueryRequest, QueryResponse> {
    serde_json::from_str::<QueryRequest>(line).map_err(|e| QueryResponse::Error {
        message: format!("bad request: {e}"),
    })
}

/// Serve exactly one connection: read a request line, run the search, and write
/// one response line back.
///
/// A closed connection (no request line) writes nothing. A malformed request
/// line still gets a well-formed error response, so a client always sees a reply.
pub fn serve_once(conn: &mut dyn Conn, db: &Db) -> Result<()> {
    let line = match conn.read_line()? {
        Some(l) => l,
        None => return Ok(()),
    };
    let response = match parse_request(&line) {
        Ok(req) => handle_request(db, &req),
        Err(err) => err,
    };
    conn.write_all(response.to_line().as_bytes())
}

/// Accept and serve connections until [`Listener::accept`] errors.
///
/// A per-connection failure (e.g. a broken pipe) is swallowed so one bad client
/// cannot take the daemon down; only an accept error ends the loop.
pub fn serve_loop<L: Listener>(listener: &L, db: &Db) -> Result<()> {
    loop {
        let mut conn = listener.accept()?;
        let _ = serve_once(&mut conn, db);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageRecord, Role, SessionMeta, Tool};
    use crate::source::{ParsedDelta, SessionFile};
    use crate::watch_shim::WatchEvent;
    use std::cell::RefCell;

    // ---- fakes -----------------------------------------------------------

    /// Clock whose value the test sets directly.
    struct FakeClock {
        now: std::cell::Cell<u64>,
    }
    impl FakeClock {
        fn at(now: u64) -> Self {
            FakeClock {
                now: std::cell::Cell::new(now),
            }
        }
        fn set(&self, now: u64) {
            self.now.set(now);
        }
    }
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.get()
        }
    }

    /// Unwrap an `Ok` response's hits, or fail. A single shared helper so the
    /// wrong-variant arm is defined once rather than at every call site.
    #[track_caller]
    fn ok_hits(resp: QueryResponse) -> Vec<ResponseHit> {
        match resp {
            QueryResponse::Ok { hits } => hits,
            QueryResponse::Error { message } => panic!("expected ok, got error: {message}"),
        }
    }

    /// Unwrap an `Error` response's message, or fail.
    #[track_caller]
    fn err_message(resp: QueryResponse) -> String {
        match resp {
            QueryResponse::Error { message } => message,
            QueryResponse::Ok { .. } => panic!("expected an error response"),
        }
    }

    /// Watcher that replays a scripted sequence of poll outcomes.
    struct FakeWatcher {
        steps: RefCell<std::collections::VecDeque<Result<Option<WatchEvent>>>>,
    }
    impl FakeWatcher {
        fn new(steps: Vec<Result<Option<WatchEvent>>>) -> Self {
            FakeWatcher {
                steps: RefCell::new(steps.into_iter().collect()),
            }
        }
    }
    impl FileWatcher for FakeWatcher {
        fn poll(&mut self, _timeout: Duration) -> Result<Option<WatchEvent>> {
            self.steps
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(crate::error::Error::other("watcher exhausted")))
        }
    }

    /// Connection that hands out one canned request line, then captures the
    /// bytes written back.
    struct FakeConn {
        request: Option<String>,
        written: Vec<u8>,
    }
    impl FakeConn {
        fn with(request: Option<&str>) -> Self {
            FakeConn {
                request: request.map(str::to_string),
                written: Vec::new(),
            }
        }
        fn response(&self) -> String {
            String::from_utf8(self.written.clone()).unwrap()
        }
    }
    impl Conn for FakeConn {
        fn read_line(&mut self) -> Result<Option<String>> {
            Ok(self.request.take())
        }
        fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
            self.written.extend_from_slice(bytes);
            Ok(())
        }
    }

    /// Listener that yields a fixed queue of connections, then errors.
    struct FakeListener {
        conns: RefCell<std::collections::VecDeque<FakeConn>>,
    }
    impl Listener for FakeListener {
        type Conn = FakeConn;
        fn accept(&self) -> Result<FakeConn> {
            self.conns
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| crate::error::Error::other("no more connections"))
        }
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            session_id: "s1".into(),
            tool: Tool::ClaudeCode,
            project_path: Some("/proj".into()),
            repo_id: Some("r".into()),
            project_name: Some("proj".into()),
            git_branch: Some("trunk".into()),
            account: None,
            first_ts: 100,
            last_ts: 100,
        }
    }

    fn msg() -> MessageRecord {
        MessageRecord {
            session_id: "s1".into(),
            tool: Tool::ClaudeCode,
            seq: 0,
            ts: 100,
            role: Role::User,
            tool_name: None,
            uuid: None,
            text: "the quick brown fox".into(),
            cwd: None,
        }
    }

    /// A source that reports one file and one message, so a flush produces
    /// observable index growth.
    struct OneShotSource;
    impl SessionSource for OneShotSource {
        fn tool(&self) -> Tool {
            Tool::ClaudeCode
        }
        fn discover(&self) -> Result<Vec<SessionFile>> {
            Ok(vec![SessionFile {
                path: "/tmp/does-not-matter.jsonl".into(),
                file_key: "k1".into(),
                size: 10,
                mtime: 1,
            }])
        }
        fn parse(&self, _f: &SessionFile, _from: u64) -> Result<ParsedDelta> {
            Ok(ParsedDelta {
                messages: vec![msg()],
                session: meta(),
                new_watermark: 10,
            })
        }
    }

    struct NoGit;
    impl GitRunner for NoGit {
        fn run(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
            Ok(String::new())
        }
    }

    fn one_source() -> Vec<Box<dyn SessionSource>> {
        vec![Box::new(OneShotSource)]
    }

    // ---- debouncer -------------------------------------------------------

    #[test]
    fn debouncer_is_not_due_without_events() {
        let mut d = Debouncer::new(50);
        assert!(!d.is_dirty());
        assert!(!d.take_due(1_000));
    }

    #[test]
    fn debouncer_coalesces_a_burst_into_one_flush() {
        let mut d = Debouncer::new(50);
        // A burst of events at 0, 10, 20 — each pushes the deadline out.
        d.record(0);
        assert!(!d.take_due(30)); // only 30ms of quiet after the last event? no: last=0
        d.record(10);
        d.record(20);
        // Not yet quiet for the full window.
        assert!(!d.take_due(60)); // 60 - 20 = 40 < 50
        assert!(d.is_dirty());
        // Quiet window elapsed: exactly one flush is due.
        assert!(d.take_due(70)); // 70 - 20 = 50 >= 50
                                 // And it does not fire again until a new event arrives.
        assert!(!d.is_dirty());
        assert!(!d.take_due(1_000));
    }

    #[test]
    fn debouncer_rearms_after_flush() {
        let mut d = Debouncer::new(10);
        d.record(0);
        assert!(d.take_due(10));
        d.record(100);
        assert!(!d.take_due(105));
        assert!(d.take_due(110));
    }

    // ---- watch loop ------------------------------------------------------

    fn fresh_db() -> Db {
        let db = Db::open(":memory:").unwrap();
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn watch_step_no_event_no_flush() {
        let db = fresh_db();
        let mut watcher = FakeWatcher::new(vec![Ok(None)]);
        let mut deb = Debouncer::new(50);
        let clock = FakeClock::at(0);
        let out = watch_step(&mut watcher, &mut deb, &clock, &one_source(), &db, &NoGit).unwrap();
        assert!(out.is_none());
        assert!(!deb.is_dirty());
    }

    #[test]
    fn watch_step_event_marks_dirty_but_waits_for_quiet() {
        let db = fresh_db();
        let mut watcher = FakeWatcher::new(vec![Ok(Some(WatchEvent::new("/x")))]);
        let mut deb = Debouncer::new(50);
        let clock = FakeClock::at(0);
        let out = watch_step(&mut watcher, &mut deb, &clock, &one_source(), &db, &NoGit).unwrap();
        assert!(out.is_none(), "quiet window not elapsed yet");
        assert!(deb.is_dirty());
    }

    #[test]
    fn watch_step_flushes_after_quiet_and_indexes() {
        let db = fresh_db();
        let sources = one_source();
        // Step 1: an event at t=0. Step 2: no event, clock advanced past window.
        let mut watcher = FakeWatcher::new(vec![Ok(Some(WatchEvent::new("/x"))), Ok(None)]);
        let mut deb = Debouncer::new(50);
        let clock = FakeClock::at(0);

        let s1 = watch_step(&mut watcher, &mut deb, &clock, &sources, &db, &NoGit).unwrap();
        assert!(s1.is_none());

        clock.set(100);
        let s2 = watch_step(&mut watcher, &mut deb, &clock, &sources, &db, &NoGit)
            .unwrap()
            .expect("a flush should have run");
        assert_eq!(s2.messages_added, 1);
        assert_eq!(s2.sessions_touched, 1);
        assert!(!deb.is_dirty());

        // The message really landed and is searchable.
        let hits = db
            .search("quick", &Scope::default(), &SearchOpts::default())
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
    }

    #[test]
    fn watch_loop_flushes_pending_burst_on_shutdown() {
        let db = fresh_db();
        let sources = one_source();
        // One event, then the watcher errors before the quiet window elapses.
        let mut watcher = FakeWatcher::new(vec![
            Ok(Some(WatchEvent::new("/x"))),
            Err(crate::error::Error::other("backend gone")),
        ]);
        let mut deb = Debouncer::new(10_000);
        let clock = FakeClock::at(0);

        let err = watch_loop(&mut watcher, &mut deb, &clock, &sources, &db, &NoGit).unwrap_err();
        assert!(err.to_string().contains("backend gone"));
        // Despite the abrupt stop, the pending burst was flushed.
        let hits = db
            .search("fox", &Scope::default(), &SearchOpts::default())
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn watch_loop_propagates_error_when_nothing_pending() {
        let db = fresh_db();
        let mut watcher = FakeWatcher::new(vec![Err(crate::error::Error::other("boom"))]);
        let mut deb = Debouncer::new(50);
        let clock = FakeClock::at(0);
        let err =
            watch_loop(&mut watcher, &mut deb, &clock, &one_source(), &db, &NoGit).unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert!(!deb.is_dirty());
    }

    // ---- request handling ------------------------------------------------

    fn seed(db: &Db) {
        let sources = one_source();
        sync(&sources, db, &NoGit).unwrap();
    }

    #[test]
    fn request_scope_lowers_every_field() {
        let rs = RequestScope {
            account_uuid: Some("a".into()),
            org_uuid: Some("o".into()),
            profile: Some("p".into()),
            tool: Some("claude-code".into()),
            repo_id: Some("r".into()),
            cwd: Some("/c".into()),
            branch: Some("b".into()),
            session_id: Some("s".into()),
            role: Some("user".into()),
            tool_name: Some("Bash".into()),
            since: Some(1),
            until: Some(2),
        };
        let s = rs.to_scope();
        assert_eq!(s.account_uuid.as_deref(), Some("a"));
        assert_eq!(s.org_uuid.as_deref(), Some("o"));
        assert_eq!(s.profile.as_deref(), Some("p"));
        assert_eq!(s.tool.as_deref(), Some("claude-code"));
        assert_eq!(s.repo_id.as_deref(), Some("r"));
        assert_eq!(s.cwd.as_deref(), Some("/c"));
        assert_eq!(s.branch.as_deref(), Some("b"));
        assert_eq!(s.session_id.as_deref(), Some("s"));
        assert_eq!(s.role.as_deref(), Some("user"));
        assert_eq!(s.tool_name.as_deref(), Some("Bash"));
        assert_eq!(s.since, Some(1));
        assert_eq!(s.until, Some(2));
    }

    #[test]
    fn parse_request_defaults_are_applied() {
        let req = parse_request(r#"{"query":"hello"}"#).unwrap();
        assert_eq!(req.query, "hello");
        assert_eq!(req.limit, 20);
        assert!(!req.code);
        assert_eq!(req.scope, RequestScope::default());
    }

    #[test]
    fn parse_request_rejects_garbage() {
        let err = parse_request("not json").unwrap_err();
        assert!(err_message(err).starts_with("bad request:"));
    }

    #[test]
    fn handle_request_returns_matching_hits() {
        let db = fresh_db();
        seed(&db);
        let req = QueryRequest {
            query: "brown".into(),
            scope: RequestScope::default(),
            code: false,
            limit: 5,
        };
        let hits = ok_hits(handle_request(&db, &req));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(hits[0].tool.as_deref(), Some("claude-code"));
        assert_eq!(hits[0].ts, 100);
        assert!(hits[0].snippet.to_lowercase().contains("brown"));
    }

    #[test]
    fn handle_request_scope_filters_out_nonmatches() {
        let db = fresh_db();
        seed(&db);
        let req = QueryRequest {
            query: "brown".into(),
            scope: RequestScope {
                tool: Some("codex".into()),
                ..RequestScope::default()
            },
            code: false,
            limit: 5,
        };
        assert!(ok_hits(handle_request(&db, &req)).is_empty());
    }

    #[test]
    fn handle_request_maps_search_error_to_error_response() {
        let db = fresh_db();
        seed(&db);
        // An unbalanced FTS quote is a query syntax error from SQLite.
        let req = QueryRequest {
            query: "\"unterminated".into(),
            scope: RequestScope::default(),
            code: false,
            limit: 5,
        };
        assert!(!err_message(handle_request(&db, &req)).is_empty());
    }

    #[test]
    fn response_to_line_is_newline_terminated_json() {
        let resp = QueryResponse::Ok { hits: vec![] };
        let line = resp.to_line();
        assert!(line.ends_with('\n'));
        let parsed: QueryResponse = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn serve_once_round_trips_request_to_response() {
        let db = fresh_db();
        seed(&db);
        let mut conn = FakeConn::with(Some(r#"{"query":"fox","limit":3}"#));
        serve_once(&mut conn, &db).unwrap();
        let resp: QueryResponse = serde_json::from_str(conn.response().trim_end()).unwrap();
        let hits = ok_hits(resp);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
    }

    #[test]
    fn serve_once_on_closed_connection_writes_nothing() {
        let db = fresh_db();
        let mut conn = FakeConn::with(None);
        serve_once(&mut conn, &db).unwrap();
        assert!(conn.response().is_empty());
    }

    #[test]
    fn serve_once_bad_request_still_replies_with_error() {
        let db = fresh_db();
        let mut conn = FakeConn::with(Some("garbage"));
        serve_once(&mut conn, &db).unwrap();
        let resp: QueryResponse = serde_json::from_str(conn.response().trim_end()).unwrap();
        assert!(err_message(resp).starts_with("bad request:"));
    }

    #[test]
    fn serve_loop_serves_each_connection_then_ends_on_accept_error() {
        let db = fresh_db();
        seed(&db);
        let listener = FakeListener {
            conns: RefCell::new(
                vec![
                    FakeConn::with(Some(r#"{"query":"quick"}"#)),
                    FakeConn::with(Some("bad")),
                ]
                .into_iter()
                .collect(),
            ),
        };
        // Loop ends when accept runs dry.
        let err = serve_loop(&listener, &db).unwrap_err();
        assert!(err.to_string().contains("no more connections"));
    }
}
