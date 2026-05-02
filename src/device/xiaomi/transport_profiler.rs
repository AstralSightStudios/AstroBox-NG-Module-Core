use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(target_arch = "wasm32")]
use web_time::{Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_EVENTS: usize = 8192;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportProfilerEvent {
    pub phase: String,
    pub label: String,
    pub at_ms: u64,
    pub duration_ms: Option<u64>,
    pub packets: Option<u32>,
    pub bytes: Option<u64>,
    pub seq: Option<u32>,
    pub success: Option<bool>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportProfilerReport {
    pub active: bool,
    pub session_started_at_epoch_ms: Option<u64>,
    pub duration_ms: u64,
    pub event_count: usize,
    pub dropped_event_count: u64,
    pub events: Vec<TransportProfilerEvent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportProfilerStatus {
    pub active: bool,
    pub session_started_at_epoch_ms: Option<u64>,
    pub duration_ms: u64,
    pub event_count: usize,
    pub dropped_event_count: u64,
}

#[derive(Debug)]
struct ActiveSession {
    started_at: Instant,
    started_at_epoch_ms: u64,
    events: Vec<TransportProfilerEvent>,
    dropped_event_count: u64,
}

#[derive(Debug)]
struct TransportProfilerState {
    active: Option<ActiveSession>,
    last_report: Option<TransportProfilerReport>,
    max_events: usize,
}

impl Default for TransportProfilerState {
    fn default() -> Self {
        Self {
            active: None,
            last_report: None,
            max_events: DEFAULT_MAX_EVENTS,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TransportProfilerHandle {
    inner: Arc<Mutex<TransportProfilerState>>,
}

impl TransportProfilerHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&self) {
        let mut state = self.inner.lock();
        state.active = Some(ActiveSession {
            started_at: Instant::now(),
            started_at_epoch_ms: now_epoch_ms(),
            events: Vec::new(),
            dropped_event_count: 0,
        });
    }

    pub fn stop(&self) -> TransportProfilerReport {
        let mut state = self.inner.lock();
        let report = if let Some(session) = state.active.take() {
            TransportProfilerReport {
                active: false,
                session_started_at_epoch_ms: Some(session.started_at_epoch_ms),
                duration_ms: saturating_elapsed_ms(session.started_at),
                event_count: session.events.len(),
                dropped_event_count: session.dropped_event_count,
                events: session.events,
            }
        } else {
            state.last_report.clone().unwrap_or_else(|| TransportProfilerReport {
                active: false,
                session_started_at_epoch_ms: None,
                duration_ms: 0,
                event_count: 0,
                dropped_event_count: 0,
                events: Vec::new(),
            })
        };
        state.last_report = Some(report.clone());
        report
    }

    pub fn status(&self) -> TransportProfilerStatus {
        let state = self.inner.lock();
        if let Some(session) = state.active.as_ref() {
            return TransportProfilerStatus {
                active: true,
                session_started_at_epoch_ms: Some(session.started_at_epoch_ms),
                duration_ms: saturating_elapsed_ms(session.started_at),
                event_count: session.events.len(),
                dropped_event_count: session.dropped_event_count,
            };
        }

        state
            .last_report
            .as_ref()
            .map(|report| TransportProfilerStatus {
                active: false,
                session_started_at_epoch_ms: report.session_started_at_epoch_ms,
                duration_ms: report.duration_ms,
                event_count: report.event_count,
                dropped_event_count: report.dropped_event_count,
            })
            .unwrap_or(TransportProfilerStatus {
                active: false,
                session_started_at_epoch_ms: None,
                duration_ms: 0,
                event_count: 0,
                dropped_event_count: 0,
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        phase: impl Into<String>,
        label: impl Into<String>,
        duration_ms: Option<u64>,
        packets: Option<u32>,
        bytes: Option<u64>,
        seq: Option<u32>,
        success: Option<bool>,
        detail: Option<String>,
    ) {
        let mut state = self.inner.lock();
        let max_events = state.max_events;
        let Some(session) = state.active.as_mut() else {
            return;
        };

        if session.events.len() >= max_events {
            session.dropped_event_count = session.dropped_event_count.saturating_add(1);
            return;
        }

        session.events.push(TransportProfilerEvent {
            phase: phase.into(),
            label: label.into(),
            at_ms: saturating_elapsed_ms(session.started_at),
            duration_ms,
            packets,
            bytes,
            seq,
            success,
            detail,
        });
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn saturating_elapsed_ms(started_at: Instant) -> u64 {
    started_at
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
