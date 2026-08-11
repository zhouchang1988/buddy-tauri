//! Coalescing of high-frequency `actor.stdout` envelopes before they are
//! emitted to the webview.
//!
//! Actor CLIs stream output in small chunks; forwarding every chunk as its
//! own `buddy:event` emit means a separate JS task on the webview main
//! thread per chunk, which competes with keyboard input and makes typing
//! stutter during active runs. Consecutive stdout chunks for the same
//! task/run are merged into a single envelope (text concatenated, latest
//! `ts`/`seq` kept) and flushed after a short window or when any other
//! event arrives, preserving cross-event ordering.

use crate::buddy::types::TaskEventEnvelope;

/// How long a pending stdout envelope may be held before it must be flushed.
pub const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);

fn is_stdout(envelope: &TaskEventEnvelope) -> bool {
    envelope.event.event_type == "actor.stdout"
}

fn same_stream(a: &TaskEventEnvelope, b: &TaskEventEnvelope) -> bool {
    a.task_id == b.task_id
        && a.workspace_key == b.workspace_key
        && a.event.run_id == b.event.run_id
        && a.event.actor == b.event.actor
}

/// Merge `incoming` into `pending`: concatenate `payload.text`, keep the
/// incoming `ts` and `seq`.
fn merge(pending: &mut TaskEventEnvelope, incoming: TaskEventEnvelope) {
    let incoming_text = incoming
        .event
        .payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    pending
        .event
        .payload
        .entry("text")
        .and_modify(|v| {
            if let Some(s) = v.as_str() {
                let mut merged = s.to_owned();
                merged.push_str(&incoming_text);
                *v = serde_json::Value::String(merged);
            }
        })
        .or_insert(serde_json::Value::String(incoming_text));
    pending.event.ts = incoming.event.ts;
    pending.event.seq = incoming.event.seq;
}

/// Holds at most one pending stdout envelope and decides what may be
/// emitted immediately.
#[derive(Default)]
pub struct StdoutCoalescer {
    pending: Option<TaskEventEnvelope>,
}

impl StdoutCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Take the pending envelope, if any (used when the flush window
    /// expires or the channel closes).
    pub fn take_pending(&mut self) -> Option<TaskEventEnvelope> {
        self.pending.take()
    }

    /// Feed the next envelope. Returns envelopes that must be emitted
    /// immediately, in order (empty when the envelope was buffered).
    pub fn push(&mut self, envelope: TaskEventEnvelope) -> Vec<TaskEventEnvelope> {
        if is_stdout(&envelope) {
            match self.pending.take() {
                Some(mut pending) if same_stream(&pending, &envelope) => {
                    merge(&mut pending, envelope);
                    self.pending = Some(pending);
                    Vec::new()
                }
                Some(pending) => {
                    // Different stream: flush the old one, buffer the new.
                    self.pending = Some(envelope);
                    vec![pending]
                }
                None => {
                    self.pending = Some(envelope);
                    Vec::new()
                }
            }
        } else {
            match self.pending.take() {
                // Flush first so ordering across event types is preserved.
                Some(pending) => vec![pending, envelope],
                None => vec![envelope],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::types::{Event, TaskEventEnvelope};

    fn envelope(task_id: &str, run_id: Option<&str>, event_type: &str, text: Option<&str>, seq: u64) -> TaskEventEnvelope {
        let mut payload = serde_json::Map::new();
        if let Some(t) = text {
            payload.insert("text".to_string(), serde_json::Value::String(t.to_string()));
        }
        TaskEventEnvelope {
            workspace_key: "ws".to_string(),
            task_id: task_id.to_string(),
            event: Event {
                seq,
                task_id: Some(task_id.to_string()),
                event_type: event_type.to_string(),
                actor: Some("kimi".to_string()),
                ts: format!("2026-01-01T00:00:{seq:02}Z"),
                run_id: run_id.map(|s| s.to_string()),
                payload,
            },
        }
    }

    fn text_of(e: &TaskEventEnvelope) -> String {
        e.event
            .payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn merges_consecutive_stdout_chunks() {
        let mut c = StdoutCoalescer::new();
        assert!(c.push(envelope("t1", Some("r1"), "actor.stdout", Some("hello "), 1)).is_empty());
        assert!(c.has_pending());
        assert!(c.push(envelope("t1", Some("r1"), "actor.stdout", Some("world"), 2)).is_empty());
        let merged = c.take_pending().expect("pending envelope");
        assert_eq!(text_of(&merged), "hello world");
        assert_eq!(merged.event.seq, 2);
        assert_eq!(merged.event.ts, "2026-01-01T00:00:02Z");
        assert!(!c.has_pending());
    }

    #[test]
    fn flushes_pending_when_other_event_arrives_in_order() {
        let mut c = StdoutCoalescer::new();
        c.push(envelope("t1", Some("r1"), "actor.stdout", Some("a"), 1));
        let out = c.push(envelope("t1", Some("r1"), "actor.done", None, 2));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.event_type, "actor.stdout");
        assert_eq!(out[1].event.event_type, "actor.done");
        assert!(!c.has_pending());
    }

    #[test]
    fn does_not_merge_across_runs_or_tasks() {
        let mut c = StdoutCoalescer::new();
        c.push(envelope("t1", Some("r1"), "actor.stdout", Some("a"), 1));
        let out = c.push(envelope("t1", Some("r2"), "actor.stdout", Some("b"), 2));
        assert_eq!(out.len(), 1);
        assert_eq!(text_of(&out[0]), "a");
        assert!(c.has_pending());

        let out = c.push(envelope("t2", Some("r2"), "actor.stdout", Some("c"), 3));
        assert_eq!(out.len(), 1);
        assert_eq!(text_of(&out[0]), "b");
        let pending = c.take_pending().expect("pending envelope");
        assert_eq!(text_of(&pending), "c");
    }

    #[test]
    fn stdout_without_text_still_coalesces() {
        let mut c = StdoutCoalescer::new();
        assert!(c.push(envelope("t1", Some("r1"), "actor.stdout", None, 1)).is_empty());
        assert!(c.push(envelope("t1", Some("r1"), "actor.stdout", Some("x"), 2)).is_empty());
        let merged = c.take_pending().expect("pending envelope");
        assert_eq!(text_of(&merged), "x");
    }

    #[test]
    fn non_stdout_passes_through_without_pending() {
        let mut c = StdoutCoalescer::new();
        let out = c.push(envelope("t1", None, "task.state", None, 1));
        assert_eq!(out.len(), 1);
        assert!(!c.has_pending());
    }
}
