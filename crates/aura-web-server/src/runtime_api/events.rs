//! The SSE body of the two events routes.

use std::future::Future;
use std::time::Duration;

use aura::journal::JournalItem;
use aura::runtime::Subscription;
use aura_events::{RunId, SequenceNumber};
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;

use super::wire::UnknownEvent;
use crate::streaming::{HEARTBEAT_FRAME, tick};

/// Renders one item of a session's stream as an SSE frame, or as `None` for
/// an item the wire has no frame for.
pub(super) type Frame = fn(&JournalItem) -> Option<Bytes>;

/// What woke the events body.
enum Wake {
    /// The subscription's next item, or its end.
    Item(Option<JournalItem>),
    /// The server is shutting down.
    Shutdown,
    /// The heartbeat's period elapsed.
    Heartbeat,
}

/// The SSE body of an events route. It reads `subscription`, renders each
/// item with `frame`, and sends a heartbeat comment every `heartbeat`, the
/// first one at once.
///
/// The body ends at the first of these:
///
/// - An item [`ends_body`] reads as the last one, for `run` and `to`. The
///   item is sent first.
/// - An event of another run, when a run is given. It is not sent. A
///   session's next run starts only after the run before it has ended, so
///   such an event means the reader lost `run`'s ending in a gap. When it
///   comes at or before `to`, the journal's numbers up to `to` belong to
///   another run: the store forgot the session and a later run numbered it
///   again. Then one gap past `to` is sent first, since none of `run`'s
///   events are left to send.
/// - The end of the subscription.
/// - `shutdown` resolving. Every item that is ready at the same time goes
///   out first.
///
/// With neither `run` nor `to`, the body is a session's stream, which has
/// no end of its own. The subscription is dropped when the body ends, which
/// detaches the observer.
pub(super) fn events_body(
    subscription: Subscription,
    frame: Frame,
    run: Option<RunId>,
    to: Option<SequenceNumber>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    heartbeat: Option<Duration>,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    let mut interval = crate::streaming::heartbeat(heartbeat);
    Box::pin(async_stream::stream! {
        let mut subscription = subscription;
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            // Items are checked first, so an item that is ready goes out
            // before a shutdown or a heartbeat that is ready at the same time.
            let wake = tokio::select! {
                biased;
                item = subscription.next() => Wake::Item(item),
                () = &mut shutdown => Wake::Shutdown,
                () = tick(&mut interval) => Wake::Heartbeat,
            };
            match wake {
                Wake::Item(None) | Wake::Shutdown => break,
                Wake::Item(Some(item)) if of_another_run(&item, run) => {
                    if let Some(to) = to
                        && item.seq() <= to
                        && let Some(gap) = frame(&JournalItem::Gap {
                            resumed_at: to.next(),
                        })
                    {
                        yield Ok(gap);
                    }
                    break;
                }
                Wake::Item(Some(item)) => {
                    let last = ends_body(&item, run, to);
                    if let Some(bytes) = frame(&item) {
                        yield Ok::<Bytes, std::io::Error>(bytes);
                    }
                    if last {
                        break;
                    }
                }
                Wake::Heartbeat => yield Ok(Bytes::from_static(HEARTBEAT_FRAME)),
            }
        }
        drop(subscription);
    })
}

/// Whether `item` is the last one a body sends: the terminal event of
/// `run`, or an item that reaches `to`. A gap that resumes at `to` does not
/// reach it, because the item at `to` comes next.
fn ends_body(item: &JournalItem, run: Option<RunId>, to: Option<SequenceNumber>) -> bool {
    let ends_run = match item {
        JournalItem::Event(event) => run.is_some() && event.run_id == run && event.ends_run(),
        JournalItem::Gap { .. } | JournalItem::Unknown { .. } => false,
    };
    let reaches_to = to.is_some_and(|to| match item {
        JournalItem::Event(_) | JournalItem::Unknown { .. } => item.seq() >= to,
        JournalItem::Gap { resumed_at } => *resumed_at > to,
    });
    ends_run || reaches_to
}

/// Whether `item` is an event of a run other than `run`, when a run is
/// given.
fn of_another_run(item: &JournalItem, run: Option<RunId>) -> bool {
    match item {
        JournalItem::Event(event) => {
            run.is_some_and(|run| event.run_id.is_some_and(|id| id != run))
        }
        JournalItem::Gap { .. } | JournalItem::Unknown { .. } => false,
    }
}

/// The frame of the envelope projection, which sends a session's events as
/// they are:
///
/// - An event is one unnamed SSE message whose data is the `SessionEvent`
///   as JSON on one line.
/// - A gap is its [`gap_message`].
/// - An entry this binary cannot decode has no `SessionEvent` to write as
///   JSON. Such an entry is one unnamed SSE message whose data is the bytes
///   the store holds.
/// - An event that fails to serialize as JSON is an SSE message named
///   `unknown` whose data is an [`UnknownEvent`] as JSON on one line: the
///   event's ids, `seq` and `at`, without its payload. An event that holds
///   a variant read as unknown fails that way.
pub(super) fn envelope_frame(item: &JournalItem) -> Option<Bytes> {
    match item {
        JournalItem::Event(event) => match serde_json::to_string(&**event) {
            Ok(json) => Some(message(None, json.as_bytes())),
            Err(error) => {
                tracing::debug!(
                    session_id = %event.session_id,
                    seq = %event.seq,
                    %error,
                    "an event could not be written as JSON; sending it as `unknown`"
                );
                let marker = serde_json::to_string(&UnknownEvent::from(&**event)).ok()?;
                Some(message(Some("unknown"), marker.as_bytes()))
            }
        },
        JournalItem::Gap { resumed_at } => Some(gap_message(*resumed_at)),
        JournalItem::Unknown { raw, .. } => Some(message(None, raw)),
    }
}

/// An SSE message named `gap` whose data is `{"resumed_at": <seq>}`, the
/// number the stream resumes at.
pub(super) fn gap_message(resumed_at: SequenceNumber) -> Bytes {
    let data = serde_json::json!({ "resumed_at": resumed_at }).to_string();
    message(Some("gap"), data.as_bytes())
}

/// One SSE message: an `event:` line when `name` is given, then a `data:`
/// line for each line of `data`, then a blank line. A client joins the
/// `data:` lines with a newline, so data that holds line breaks reaches it
/// whole; a line break left inside one `data:` line would end the field and
/// leave the rest unread.
fn message(name: Option<&str>, data: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(data.len() + 32);
    if let Some(name) = name {
        frame.extend_from_slice(b"event: ");
        frame.extend_from_slice(name.as_bytes());
        frame.push(b'\n');
    }
    for line in sse_lines(data) {
        frame.extend_from_slice(b"data: ");
        frame.extend_from_slice(line);
        frame.push(b'\n');
    }
    frame.push(b'\n');
    Bytes::from(frame)
}

/// `data` cut at each line break SSE recognises: `\r\n`, `\n` and a lone
/// `\r`. Data with no line break is one line, the empty data included.
fn sse_lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = Some(data);
    std::iter::from_fn(move || {
        let current = rest?;
        match current.iter().position(|&b| b == b'\n' || b == b'\r') {
            Some(at) => {
                let width = if current[at..].starts_with(b"\r\n") {
                    2
                } else {
                    1
                };
                rest = Some(&current[at + width..]);
                Some(&current[..at])
            }
            None => {
                rest = None;
                Some(current)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aura::journal::JournalItem;
    use aura_events::run::{LifecycleEvent, SessionEvent, SessionEventPayload};
    use aura_events::{RunId, SequenceNumber, SessionId, Timestamp};
    use bytes::Bytes;
    use serde_json::{Value, json};

    use super::envelope_frame;

    /// The time every event here is stamped with, in Unix milliseconds.
    const AT: u64 = 1_763_568_000_000;

    fn seq(n: u64) -> SequenceNumber {
        SequenceNumber::try_from(n).expect("a sequence number")
    }

    /// An event of the session `c-42:sre:0`, numbered `number`.
    fn event(run_id: Option<RunId>, number: u64, payload: SessionEventPayload) -> JournalItem {
        JournalItem::Event(Arc::new(SessionEvent {
            session_id: SessionId::new("c-42:sre:0").expect("a session id"),
            run_id,
            seq: seq(number),
            at: Timestamp::from_unix_millis(AT),
            payload,
        }))
    }

    /// The value of an SSE line for `name`: what follows its colon, less one
    /// leading space.
    fn field_of<'a>(line: &'a str, name: &str) -> Option<&'a str> {
        let value = line.strip_prefix(name)?.strip_prefix(':')?;
        Some(value.strip_prefix(' ').unwrap_or(value))
    }

    /// The frame's `event:` name, if it has one, and its data as JSON, read
    /// as an SSE client reads them. Panics unless the frame is one message
    /// that ends in a blank line and holds one `data:` line.
    fn message_of(frame: &Bytes) -> (Option<String>, Value) {
        let frame = std::str::from_utf8(frame).expect("UTF-8");
        let text = frame
            .strip_suffix("\n\n")
            .unwrap_or_else(|| panic!("a frame ends in a blank line: {frame:?}"));
        let mut lines = text.split('\n');
        let first = lines.next().unwrap_or_default();
        let (name, data_line) = match field_of(first, "event") {
            Some(name) => (Some(name.to_owned()), lines.next().unwrap_or_default()),
            None => (None, first),
        };
        let data = field_of(data_line, "data")
            .unwrap_or_else(|| panic!("a frame has a data line: {frame:?}"));
        assert_eq!(lines.next(), None, "a frame has one data line: {frame:?}");
        let data = serde_json::from_str(data)
            .unwrap_or_else(|error| panic!("the data is JSON ({error}): {frame:?}"));
        (name, data)
    }

    #[test]
    fn an_event_is_one_unnamed_message_whose_data_is_the_event_as_json() {
        let run = RunId::mint();
        let item = event(
            Some(run),
            17,
            SessionEventPayload::Lifecycle(LifecycleEvent::ClaimsExhausted),
        );

        let frame = envelope_frame(&item).expect("an event has a frame");
        assert_eq!(
            message_of(&frame),
            (
                None,
                json!({
                    "session_id": "c-42:sre:0",
                    "run_id": run.to_string(),
                    "seq": 17,
                    "at": AT,
                    "payload": { "kind": "lifecycle", "event": { "type": "claims_exhausted" } },
                })
            )
        );
    }

    #[test]
    fn a_gap_is_a_gap_message_naming_where_the_stream_resumes() {
        let frame =
            envelope_frame(&JournalItem::Gap { resumed_at: seq(9) }).expect("a gap has a frame");
        assert_eq!(
            message_of(&frame),
            (Some("gap".to_owned()), json!({ "resumed_at": 9 }))
        );
    }

    /// The stored bytes go out as an event's JSON does: one unnamed message
    /// whose data they are.
    #[test]
    fn an_entry_this_binary_cannot_decode_is_sent_as_the_bytes_the_store_holds() {
        const RAW: &str =
            r#"{"session_id":"c-42:sre:0","seq":5,"at":1,"payload":{"kind":"later","event":{}}}"#;
        let frame = envelope_frame(&JournalItem::Unknown {
            seq: seq(5),
            raw: Bytes::from_static(RAW.as_bytes()),
        })
        .expect("an undecodable entry has a frame");
        assert_eq!(frame, format!("data: {RAW}\n\n").as_bytes());
    }

    /// Stored bytes that hold line breaks go out as one `data:` line for
    /// each of their lines, which a client joins with a newline. Every line
    /// break SSE recognises ends a line: `\r\n`, `\n` and a lone `\r`.
    #[test]
    fn an_entry_with_line_breaks_is_one_data_line_for_each() {
        let frame = envelope_frame(&JournalItem::Unknown {
            seq: seq(5),
            raw: Bytes::from_static(b"{\n  \"a\": 1,\r\n  \"b\": 2\r}"),
        })
        .expect("an undecodable entry has a frame");
        assert_eq!(
            frame,
            "data: {\ndata:   \"a\": 1,\ndata:   \"b\": 2\ndata: }\n\n".as_bytes()
        );
    }

    /// The event decodes, but its lifecycle variant is one this binary
    /// reads as unknown, which cannot be written as JSON.
    #[test]
    fn an_event_with_an_inner_unknown_is_an_unknown_message_with_its_envelope() {
        let run = RunId::mint();
        let item = event(
            Some(run),
            17,
            SessionEventPayload::Lifecycle(LifecycleEvent::Unknown),
        );

        let frame = envelope_frame(&item).expect("the event has a frame");
        assert_eq!(
            message_of(&frame),
            (
                Some("unknown".to_owned()),
                json!({
                    "session_id": "c-42:sre:0",
                    "run_id": run.to_string(),
                    "seq": 17,
                    "at": AT,
                })
            )
        );
    }

    #[test]
    fn an_unknown_message_for_an_event_with_no_run_has_no_run_id() {
        let item = event(
            None,
            18,
            SessionEventPayload::Lifecycle(LifecycleEvent::Unknown),
        );

        let frame = envelope_frame(&item).expect("the event has a frame");
        assert_eq!(
            message_of(&frame),
            (
                Some("unknown".to_owned()),
                json!({ "session_id": "c-42:sre:0", "seq": 18, "at": AT })
            )
        );
    }
}
