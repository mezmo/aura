//! The queries and bodies of the runtime's routes.

use aura::RunSession;
use aura::journal::Cursor;
use aura::runtime::{
    AgentState, AgentStateKind, Attach, ObserverCounts, PageLimit, RunPage, RunQuery, RunSnapshot,
    SessionPage, SessionQuery, SessionSnapshot,
};
use aura::session_store::{RunDetails, RunFacts, RunStatus};
use aura_events::run::SessionEvent;
use aura_events::{RunId, SequenceNumber, SessionId, Timestamp};
use serde::{Deserialize, Serialize};

/// The id the client knows a run's session by: the chat id of the
/// conversation the run is on, or `None` for a run on a session that is not
/// a conversation's.
fn client_session_id(session: &RunSession) -> Option<&SessionId> {
    session.lane().map(|lane| lane.conversation().chat())
}

/// The query of `GET /v1/sessions`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListSessionsQuery {
    pub state: Option<AgentStateKind>,
    /// The agent of the session's newest run, by its `Config::agent_id`.
    pub agent: Option<String>,
    /// The id the client knows the session by.
    pub client_session_id: Option<SessionId>,
    pub limit: Option<PageLimit>,
    /// The `next_cursor` of the page before.
    pub cursor: Option<SessionId>,
}

impl From<ListSessionsQuery> for SessionQuery {
    fn from(query: ListSessionsQuery) -> Self {
        Self {
            state: query.state,
            agent: query.agent,
            chat: query.client_session_id,
            limit: query.limit.unwrap_or_default(),
            after: query.cursor,
        }
    }
}

/// The body of `GET /v1/sessions`.
#[derive(Debug, Serialize)]
pub struct ListSessionsResponse<'a> {
    sessions: Vec<SessionSummary<'a>>,
    next_cursor: Option<&'a SessionId>,
}

impl<'a> From<&'a SessionPage> for ListSessionsResponse<'a> {
    fn from(page: &'a SessionPage) -> Self {
        Self {
            sessions: page.sessions().iter().map(SessionSummary::from).collect(),
            next_cursor: page.next(),
        }
    }
}

/// One session, as `GET /v1/sessions` and `GET /v1/sessions/{id}` show it.
#[derive(Debug, Serialize)]
pub struct SessionSummary<'a> {
    /// The runtime's id for the session.
    session_id: SessionId,
    /// The id the client knows the session by. For a chat it is the chat
    /// session id, which the client sent or the server generated for it.
    client_session_id: Option<&'a SessionId>,
    /// The agent of the session's newest run, by its `Config::agent_id`.
    agent: &'a str,
    #[serde(flatten)]
    state: &'a AgentState,
    current_run: Option<RunId>,
    last_run: Option<RunId>,
    /// The sequence number of the newest event in the session's stream.
    latest_event: Option<SequenceNumber>,
    observers: ObserverCounts,
}

impl<'a> From<&'a SessionSnapshot> for SessionSummary<'a> {
    fn from(session: &'a SessionSnapshot) -> Self {
        Self {
            session_id: session.id(),
            client_session_id: client_session_id(session.run_session()),
            agent: session.agent(),
            state: session.state(),
            current_run: session.current_run(),
            last_run: session.last_run(),
            latest_event: session.latest_event(),
            observers: session.observers(),
        }
    }
}

/// The query of `GET /v1/runs`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRunsQuery {
    pub session_id: Option<SessionId>,
    /// The id the client knows the run's session by.
    pub client_session_id: Option<SessionId>,
    pub status: Option<RunStatus>,
    pub pending_approval: Option<bool>,
    pub limit: Option<PageLimit>,
    /// The `next_cursor` of the page before.
    pub cursor: Option<RunId>,
}

impl From<ListRunsQuery> for RunQuery {
    fn from(query: ListRunsQuery) -> Self {
        Self {
            session: query.session_id,
            chat: query.client_session_id,
            status: query.status,
            pending_approval: query.pending_approval,
            limit: query.limit.unwrap_or_default(),
            after: query.cursor,
        }
    }
}

/// The body of `GET /v1/runs`.
#[derive(Debug, Serialize)]
pub struct ListRunsResponse<'a> {
    runs: Vec<RunSummary<'a>>,
    next_cursor: Option<RunId>,
}

impl<'a> From<&'a RunPage> for ListRunsResponse<'a> {
    fn from(page: &'a RunPage) -> Self {
        Self {
            runs: page.runs().iter().map(RunSummary::from).collect(),
            next_cursor: page.next(),
        }
    }
}

/// One run, as `GET /v1/runs`, `GET /v1/runs/{id}` and
/// `POST /v1/runs/{id}/cancel` show it.
#[derive(Debug, Serialize)]
pub struct RunSummary<'a> {
    #[serde(flatten)]
    run: &'a RunDetails,
    /// The runtime's id for the session the run is on.
    session_id: SessionId,
    /// The id the client knows the run's session by, as on a
    /// [`SessionSummary`].
    client_session_id: Option<&'a SessionId>,
    /// The observers of the run's session.
    observers: ObserverCounts,
}

impl<'a> From<&'a RunSnapshot> for RunSummary<'a> {
    fn from(run: &'a RunSnapshot) -> Self {
        // Both fields are named, so a field added to `RunFacts` does not
        // compile here until it is given a place on the wire or left off.
        // A field added to `RunDetails` is on the wire as it is.
        let RunFacts {
            session,
            run: details,
        } = run.facts();
        Self {
            run: details,
            session_id: session.id(),
            client_session_id: client_session_id(session),
            observers: run.observers(),
        }
    }
}

/// The sequence number of the last event a client has seen on a session's
/// stream, or none for a client that has seen nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "u64")]
pub struct LastSeenEvent(Option<SequenceNumber>);

impl LastSeenEvent {
    pub fn get(self) -> Option<SequenceNumber> {
        self.0
    }
}

impl From<u64> for LastSeenEvent {
    /// 0 is no sequence number.
    fn from(after: u64) -> Self {
        Self(SequenceNumber::try_from(after).ok())
    }
}

/// The query of `GET /v1/sessions/{id}/events` and
/// `GET /v1/runs/{id}/events`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsQuery {
    /// Left out, it is none.
    #[serde(default)]
    pub after: LastSeenEvent,
    /// Whether the observer claims the session.
    #[serde(default)]
    pub claim: bool,
    /// Whether a human is at the observer.
    #[serde(default)]
    pub presence: bool,
}

impl EventsQuery {
    /// The observer the query asks for, reading from `from`: a claimant
    /// with `claim=true` and a collecting observer otherwise, with presence
    /// when `presence=true`.
    pub fn attach(&self, from: Cursor) -> Attach {
        let observer = if self.claim {
            Attach::claiming()
        } else {
            Attach::collecting()
        };
        let observer = if self.presence {
            observer.present()
        } else {
            observer
        };
        observer.from(from)
    }
}

/// The data of an `unknown` message on an events stream: an event's
/// envelope without its payload.
#[derive(Debug, Serialize)]
pub struct UnknownEvent<'a> {
    session_id: &'a SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<RunId>,
    seq: SequenceNumber,
    at: Timestamp,
}

impl<'a> From<&'a SessionEvent> for UnknownEvent<'a> {
    fn from(event: &'a SessionEvent) -> Self {
        Self {
            session_id: &event.session_id,
            run_id: event.run_id,
            seq: event.seq,
            at: event.at,
        }
    }
}

/// The body of `POST /v1/runs/{id}/cancel`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRunRequest {
    /// Why the caller is cancelling the run, in its own words.
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use aura::journal::Cursor;
    use aura::runtime::{AgentStateKind, Attach, PageLimit, RunQuery, SessionQuery};
    use aura::session_store::RunStatus;
    use aura_events::{RunId, SequenceNumber, SessionId};
    use axum::extract::Query;
    use axum::extract::rejection::QueryRejection;
    use axum::http::Uri;
    use serde::de::DeserializeOwned;

    use super::{CancelRunRequest, EventsQuery, LastSeenEvent, ListRunsQuery, ListSessionsQuery};

    fn session_id(id: &str) -> SessionId {
        SessionId::new(id).expect("a session id")
    }

    fn seq(n: u64) -> SequenceNumber {
        SequenceNumber::try_from(n).expect("a sequence number")
    }

    /// The query string `query` read as a route reads it.
    fn parsed<T: DeserializeOwned>(query: &str) -> Result<T, QueryRejection> {
        let uri: Uri = format!("/v1/route?{query}").parse().expect("a URI");
        Query::<T>::try_from_uri(&uri).map(|Query(parsed)| parsed)
    }

    fn session_query(query: &str) -> SessionQuery {
        SessionQuery::from(
            parsed::<ListSessionsQuery>(query)
                .unwrap_or_else(|error| panic!("`{query}` parses: {error}")),
        )
    }

    fn run_query(query: &str) -> RunQuery {
        RunQuery::from(
            parsed::<ListRunsQuery>(query)
                .unwrap_or_else(|error| panic!("`{query}` parses: {error}")),
        )
    }

    fn events_query(query: &str) -> EventsQuery {
        parsed::<EventsQuery>(query).unwrap_or_else(|error| panic!("`{query}` parses: {error}"))
    }

    #[test]
    fn a_session_listing_query_moves_into_the_runtimes_query() {
        assert_eq!(
            session_query(
                "state=blocked&agent=sre&client_session_id=c1&limit=5&cursor=c1%3Asre%3A0"
            ),
            SessionQuery {
                state: Some(AgentStateKind::Blocked),
                agent: Some("sre".to_owned()),
                chat: Some(session_id("c1")),
                limit: PageLimit::from(5),
                after: Some(session_id("c1:sre:0")),
            }
        );
    }

    #[test]
    fn an_empty_session_listing_query_asks_for_the_first_page_of_100() {
        let query = session_query("");
        assert_eq!(query, SessionQuery::default());
        assert_eq!(query.limit.get(), 100);
    }

    #[test]
    fn each_state_parses_from_its_name() {
        for (name, kind) in [
            ("preparing", AgentStateKind::Preparing),
            ("running", AgentStateKind::Running),
            ("blocked", AgentStateKind::Blocked),
            ("stopping", AgentStateKind::Stopping),
            ("parked", AgentStateKind::Parked),
            ("done", AgentStateKind::Done),
        ] {
            assert_eq!(session_query(&format!("state={name}")).state, Some(kind));
        }
    }

    /// A limit outside 1 to 1000 is held to the nearer end. One that is not
    /// a `u32` at all is refused.
    #[test]
    fn a_limit_outside_the_bounds_is_held_to_them() {
        assert_eq!(session_query("limit=5000").limit, PageLimit::MAX);
        assert_eq!(session_query("limit=0").limit.get(), 1);
        assert_eq!(run_query("limit=5000").limit, PageLimit::MAX);
        assert_eq!(run_query("limit=0").limit.get(), 1);
        assert!(parsed::<ListSessionsQuery>("limit=4294967296").is_err());
        assert!(parsed::<ListRunsQuery>("limit=4294967296").is_err());
    }

    /// A lane's session id has `:` in it, and `%3A` where its agent's id
    /// has a `:`. In a query each is percent-encoded once more, and the
    /// query decodes it once.
    #[test]
    fn a_cursor_is_decoded_once() {
        assert_eq!(
            session_query("cursor=c1%3Amy%253Aagent%3A0").after,
            Some(session_id("c1:my%3Aagent:0"))
        );
    }

    #[test]
    fn a_session_listing_query_refuses_what_it_cannot_parse() {
        for query in [
            "state=runing",
            "staet=running",
            "limit=abc",
            "limit=-1",
            "cursor=",
            "client_session_id=",
        ] {
            assert!(
                parsed::<ListSessionsQuery>(query).is_err(),
                "`{query}` is refused"
            );
        }
    }

    #[test]
    fn a_run_listing_query_moves_into_the_runtimes_query() {
        let cursor = RunId::mint();
        assert_eq!(
            run_query(&format!(
                "session_id=s1&client_session_id=c1&status=running&pending_approval=true&limit=7&cursor={cursor}"
            )),
            RunQuery {
                session: Some(session_id("s1")),
                chat: Some(session_id("c1")),
                status: Some(RunStatus::Running),
                pending_approval: Some(true),
                limit: PageLimit::from(7),
                after: Some(cursor),
            }
        );
        assert_eq!(run_query(""), RunQuery::default());
    }

    #[test]
    fn a_run_listing_query_refuses_what_it_cannot_parse() {
        for query in [
            "status=nope",
            "pending_approval=yes",
            "cursor=nope",
            "limit=abc",
            "session_id=",
            "sesion_id=s1",
        ] {
            assert!(
                parsed::<ListRunsQuery>(query).is_err(),
                "`{query}` is refused"
            );
        }
    }

    /// `after=0` and no `after` both read as none.
    #[test]
    fn an_events_query_reads_after_claim_and_presence() {
        for query in ["", "after=0"] {
            let read = events_query(query);
            assert_eq!(read.after, LastSeenEvent::default(), "`{query}`");
            assert_eq!(read.after.get(), None, "`{query}`");
            assert!(!read.claim && !read.presence, "`{query}`");
        }
        let read = events_query("after=12&claim=true&presence=true");
        assert_eq!(read.after.get(), Some(seq(12)));
        assert!(read.claim && read.presence);
    }

    #[test]
    fn an_events_query_refuses_what_it_cannot_parse() {
        for query in [
            "after=x",
            "after=-1",
            "claim=yes",
            "presence=1x",
            "format=chat",
        ] {
            assert!(
                parsed::<EventsQuery>(query).is_err(),
                "`{query}` is refused"
            );
        }
    }

    /// `Attach` has no equality, so the observer each query asks for is
    /// compared by its debug form, which shows every field.
    #[test]
    fn an_events_query_asks_for_the_observer_its_claim_and_presence_say() {
        let from = Cursor::After(seq(3));
        for (query, expected) in [
            ("", Attach::collecting().from(from)),
            ("presence=true", Attach::collecting().present().from(from)),
            ("claim=true", Attach::claiming().from(from)),
            (
                "claim=true&presence=true",
                Attach::claiming().present().from(from),
            ),
        ] {
            assert_eq!(
                format!("{:?}", events_query(query).attach(from)),
                format!("{expected:?}"),
                "`{query}`"
            );
        }
    }

    #[test]
    fn a_cancel_body_takes_a_reason_and_nothing_else() {
        let read = |body: &str| serde_json::from_str::<CancelRunRequest>(body);
        assert_eq!(read("{}").expect("an empty body parses").reason, None);
        assert_eq!(
            read(r#"{"reason":"the user asked"}"#)
                .expect("a reason parses")
                .reason
                .as_deref(),
            Some("the user asked")
        );
        assert!(read(r#"{"message":"x"}"#).is_err());
    }
}
