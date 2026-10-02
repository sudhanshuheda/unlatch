//! Engine events as JSON for the host's event callback.
//!
//! ```json
//! {"type": "WorkingSetChanged", "domain": "devbox", "anchor": [1, 2, …]}
//! {"type": "ErrorResolved",     "domain": "devbox"}
//! {"type": "Reimport",          "domain": "devbox", "below": 1}
//! {"type": "NeedsUser",         "domain": "devbox", "reason": "…", "url": null}
//! {"type": "StatusChanged",     "domain": "devbox", "status": <EngineStatus>}
//! ```
//! `ReplicaChanged` is not forwarded: it exists for the FUSE frontend, fires for every replica
//! change (including non-materialized containers) and the macOS host has no use for it.

use serde::Serialize;
use unlatch_core::EngineEvent;
use unlatch_proto::ipc::EngineStatus;
use unlatch_proto::ItemId;

#[derive(Serialize)]
#[serde(tag = "type")]
enum EventJson<'a> {
    WorkingSetChanged {
        domain: &'a str,
        anchor: &'a [u8],
    },
    ErrorResolved {
        domain: &'a str,
    },
    Reimport {
        domain: &'a str,
        below: ItemId,
    },
    NeedsUser {
        domain: &'a str,
        reason: &'a str,
        url: Option<&'a str>,
    },
    StatusChanged {
        domain: &'a str,
        status: &'a EngineStatus,
    },
}

/// JSON for `event`, or `None` when the event is not forwarded to the host.
pub fn event_json(domain: &str, event: &EngineEvent) -> Option<String> {
    let j = match event {
        EngineEvent::WorkingSetChanged { anchor } => {
            EventJson::WorkingSetChanged { domain, anchor }
        }
        EngineEvent::ReplicaChanged { .. } => return None,
        EngineEvent::StatusChanged(status) => EventJson::StatusChanged { domain, status },
        EngineEvent::ErrorResolved => EventJson::ErrorResolved { domain },
        EngineEvent::Reimport { below } => EventJson::Reimport {
            domain,
            below: *below,
        },
        EngineEvent::NeedsUser { reason, url } => EventJson::NeedsUser {
            domain,
            reason,
            url: url.as_deref(),
        },
    };
    match serde_json::to_string(&j) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(target: "unlatch_ffi", "event serialization failed: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use unlatch_proto::ipc::ConnState;

    fn j(e: &EngineEvent) -> Value {
        serde_json::from_str(&event_json("d", e).unwrap()).unwrap()
    }

    #[test]
    fn shapes() {
        assert_eq!(
            j(&EngineEvent::WorkingSetChanged { anchor: vec![1, 2] }),
            json!({"type": "WorkingSetChanged", "domain": "d", "anchor": [1, 2]})
        );
        assert_eq!(
            j(&EngineEvent::ErrorResolved),
            json!({"type": "ErrorResolved", "domain": "d"})
        );
        assert_eq!(
            j(&EngineEvent::Reimport {
                below: ItemId::ROOT
            }),
            json!({"type": "Reimport", "domain": "d", "below": 1})
        );
        assert_eq!(
            j(&EngineEvent::NeedsUser {
                reason: "host key".into(),
                url: Some("https://x".into())
            }),
            json!({"type": "NeedsUser", "domain": "d", "reason": "host key", "url": "https://x"})
        );
        let status = EngineStatus {
            state: ConnState::Live,
            entries: 3,
            anchor: vec![9],
            rtt_us: Some(40_000),
            cache_bytes: 0,
            pending_uploads: 0,
            server: None,
        };
        let v = j(&EngineEvent::StatusChanged(status));
        assert_eq!(v["type"], "StatusChanged");
        assert_eq!(v["status"]["state"], "Live");
        assert_eq!(v["status"]["rtt_us"], 40_000);
        assert!(event_json(
            "d",
            &EngineEvent::ReplicaChanged {
                ids: vec![],
                parents: vec![]
            }
        )
        .is_none());
    }
}
