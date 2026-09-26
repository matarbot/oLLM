//! Scripted fake backend for protocol tests (T1).
//!
//! Records every call in order and drives slot state from a small script,
//! so the seed dance's protocol invariants can be asserted on the call
//! sequence without a live backend. It also models the *backend's* view of
//! a slot (position + processing flag) so the position-confirmation step
//! of the dance is exercised for real.

use crate::backend::{Backend, ChatOutcome, SlotState};
use futures_util::{stream, Stream};
use serde_json::Value;
use std::pin::Pin;
use std::sync::Mutex;

/// One recorded backend call, in call order.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Slots,
    PrefillOnly { slot: u32, n_msgs: usize },
    Save { slot: u32, name: String },
    Restore { slot: u32, name: String },
    Chat { n_msgs: usize },
}

/// A scripted fake. `slots` is the backend's truth; `calls` is the
/// record the tests assert on.
pub struct FakeBackend {
    pub slots: Mutex<Vec<SlotState>>,
    pub calls: Mutex<Vec<Call>>,
    /// When set, `prefill_only` moves the slot's position by this many
    /// tokens (models the prompt landing at a position).
    pub prefill_delta: std::sync::atomic::AtomicU64,
    /// When set, `save_slot` returns an error (mid-seed failure).
    pub fail_save: std::sync::atomic::AtomicBool,
    /// When set, `prefill_only` returns an error.
    pub fail_prefill: std::sync::atomic::AtomicBool,
}

impl FakeBackend {
    pub fn with_slots(slots: Vec<SlotState>) -> Self {
        Self {
            slots: Mutex::new(slots),
            calls: Mutex::new(Vec::new()),
            prefill_delta: std::sync::atomic::AtomicU64::new(0),
            fail_save: std::sync::atomic::AtomicBool::new(false),
            fail_prefill: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn record(&self, c: Call) {
        self.calls.lock().unwrap().push(c);
    }

    /// The recorded call sequence.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::with_slots(Vec::new())
    }
}

impl Backend for FakeBackend {
    async fn slots(&self) -> Result<Vec<SlotState>, String> {
        self.record(Call::Slots);
        Ok(self.slots.lock().unwrap().clone())
    }

    async fn prefill_only(&self, slot: u32, messages: &Value) -> Result<Value, String> {
        let n = messages.as_array().map(|a| a.len()).unwrap_or(0);
        self.record(Call::PrefillOnly { slot, n_msgs: n });
        if self.fail_prefill.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("simulated prefill failure".into());
        }
        // Model the prompt landing: advance the slot's position.
        let delta = self.prefill_delta.fetch_add(0, std::sync::atomic::Ordering::Relaxed);
        let mut g = self.slots.lock().unwrap();
        if let Some(s) = g.iter_mut().find(|s| s.id == slot) {
            s.n_tokens = s.n_tokens.saturating_add(delta);
            s.is_processing = false;
        }
        Ok(Value::Object(Default::default()))
    }

    async fn save_slot(&self, id: u32, name: &str) -> Result<Value, String> {
        self.record(Call::Save { slot: id, name: name.to_string() });
        if self.fail_save.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("simulated save failure".into());
        }
        Ok(Value::Object(Default::default()))
    }

    async fn restore_slot(&self, id: u32, name: &str) -> Result<Value, String> {
        self.record(Call::Restore { slot: id, name: name.to_string() });
        Ok(Value::Object(Default::default()))
    }

    async fn chat(&self, body: &Value) -> Result<ChatOutcome, String> {
        let n = body
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        self.record(Call::Chat { n_msgs: n });
        let empty: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, String>> + Send>> =
            Box::pin(stream::empty::<Result<bytes::Bytes, String>>());
        Ok(ChatOutcome { status: 200, stream: empty })
    }
}
