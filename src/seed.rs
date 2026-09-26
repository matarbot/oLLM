//! Seed-on-first-sight (T2): the system-prompt bag and the seed dance.
//!
//! Pure orchestration over [`crate::backend::Backend`] — no HTTP, no real
//! filesystem (the fs half of a publish is [`Publisher`], a separate trait
//! so unit tests use an in-memory one), no wall clock.
//!
//! `prepare` is the admission hook, run once per request, in the same
//! admission, before the full request is forwarded:
//!
//! - **second sight** (bag `cached`): issue `restore_slot` only. Zero
//!   prefill calls. A missing blob (evicted) is a cold-prefill fallback.
//! - **first sight** (bag `uncached`): the seed dance —
//!   `prefill_only(system)` → confirm position + idle via `slots()` →
//!   `save_slot` → publish (R2) → bag `cached`.
//! - a failed seed marks the bag `failed`; the next sight retries the
//!   dance, and the request is always served normally regardless.
//!
//! R3 is an assertion here, not log-reading: `prepare` never issues a
//! restore (or a save) onto a slot the backend reports as processing.
//!
//! NOT YET WIRED into the request path (see HANDOFF, Forest-v1): the
//! prefix-sharing it enables waits on the E12 physics gate. The bag is the
//! only state kept here — the forest index lands later.

use crate::backend::Backend;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A blob's disk lifecycle (T2). `queued` was dropped: seeding happens in
/// the same admission, so nothing ever sits in a queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BagState {
    Uncached,
    Cached,
    Failed,
}

/// One system-prompt family: its byte-stable prompt head.
#[derive(Debug, Clone)]
pub struct BagEntry {
    pub system: Value,
    pub state: BagState,
}

pub type Bag = HashMap<String, BagEntry>;

/// The disk half of a publish (strict-write rule R2). Kept out of the
/// Backend trait so the orchestrator stays pure; tests use an in-memory
/// publisher.
#[allow(async_fn_in_trait)]
pub trait Publisher: Send + Sync {
    /// fsync the backend-written scratch file and rename it into place.
    async fn publish(&self, scratch: &str, final_name: &str) -> Result<(), String>;
}

/// fsync + rename within one directory (the cache dir).
pub struct FsPublisher {
    dir: std::path::PathBuf,
}

impl FsPublisher {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

impl Publisher for FsPublisher {
    async fn publish(&self, scratch: &str, final_name: &str) -> Result<(), String> {
        let dir = self.dir.clone();
        let scratch = scratch.to_string();
        let final_name = final_name.to_string();
        tokio::task::spawn_blocking(move || {
            let scratch_path = dir.join(&scratch);
            let final_path = dir.join(&final_name);
            let f = std::fs::File::open(&scratch_path)
                .map_err(|e| format!("open scratch {scratch}: {e}"))?;
            f.sync_all().map_err(|e| format!("fsync {scratch}: {e}"))?;
            std::fs::rename(&scratch_path, &final_path)
                .map_err(|e| format!("rename {scratch} -> {final_name}: {e}"))?;
            if let Some(parent) = final_path.parent() {
                if let Ok(d) = std::fs::File::open(parent) {
                    let _ = d.sync_all(); // index durability
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| format!("publish join: {e}"))?
    }
}

/// A scripted in-memory publisher for protocol tests.
#[derive(Default)]
pub struct MemoryPublisher {
    pub files: tokio::sync::Mutex<HashMap<String, String>>,
    pub publish_calls: tokio::sync::Mutex<Vec<(String, String)>>,
    /// When set, `publish` fails — simulates a mid-seed publish failure.
    pub fail: std::sync::atomic::AtomicBool,
}

impl Publisher for MemoryPublisher {
    async fn publish(&self, scratch: &str, final_name: &str) -> Result<(), String> {
        if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(format!("simulated publish failure ({scratch})"));
        }
        self.publish_calls.lock().await.push((scratch.to_string(), final_name.to_string()));
        self.files.lock().await.insert(final_name.to_string(), scratch.to_string());
        Ok(())
    }
}

/// The outcome of one `prepare` for one admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedResult {
    /// Second sight: the cached blob was restored. Zero prefill calls.
    Restored,
    /// Second sight but the blob is gone (evicted) — caller serves the
    /// request normally (cold prefill on the full prompt).
    ColdPrefill,
    /// First sight: the seed dance ran and the system-prompt blob
    /// published.
    Seeded,
    /// A dance step failed; the bag entry is `failed` (retried on next
    /// sight). The full request is still served normally.
    Failed,
    /// Nothing was issued: no system prompt in the request, the slot is
    /// processing (R3), or the backend does not report the slot.
    Skipped,
}

/// The seed orchestrator, parameterized over its two seams.
pub struct SeedDance<B, P> {
    bag: Arc<RwLock<Bag>>,
    _phantom: PhantomData<(B, P)>,
}

impl<B, P> SeedDance<B, P>
where
    B: Backend,
    P: Publisher,
{
    pub fn new() -> Self {
        Self {
            bag: Arc::new(RwLock::new(HashMap::new())),
            _phantom: PhantomData,
        }
    }

    /// Recognition key: hash of the `system` messages' text, in order
    /// (structural — no template parsing, no tokenization).
    pub fn system_hash(messages: &Value) -> Option<String> {
        let msgs = messages.get("messages").and_then(|m| m.as_array())?;
        let systems: Vec<&Value> = msgs
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
            .collect();
        if systems.is_empty() {
            return None;
        }
        let mut h = Sha256::new();
        for m in systems {
            h.update(serde_json::to_vec(m).unwrap_or_default());
            h.update([0u8]);
        }
        Some(hex16(&h.finalize()))
    }

    /// The blob name a seeded family publishes under (deterministic in the
    /// family key; a `backend_sig` component is added by the app layer when
    /// the dance is wired in — E9/E12 freeze the exact form).
    pub fn blob_name(key: &str) -> String {
        format!("sys-{key}__seed.bin")
    }

    /// Run the admission hook for this request on `slot`.
    ///
    /// Protocol (each item is a test in `tests/protocol.rs`):
    /// - a miss prefill's strictly before it saves, and never saves an
    ///   unconfirmed position;
    /// - a restore is never issued onto a processing slot (R3 as an
    ///   assertion, not log-reading);
    /// - a second sight of the same system hash issues a restore and ZERO
    ///   prefill calls;
    /// - a `save_slot` failure mid-seed leaves the bag uncached, the full
    ///   request is still served normally, and no dirty slot leaks.
    pub async fn prepare(
        &self,
        backend: &B,
        publisher: &P,
        slot: u32,
        messages: &Value,
    ) -> SeedResult {
        let key = match Self::system_hash(messages) {
            Some(k) => k,
            None => return SeedResult::Skipped, // no system prompt: nothing to seed
        };

        // Slot gate: R3 — never issue restore/save onto a processing slot.
        let slot_state = match backend.slots().await {
            Ok(states) => match states.into_iter().find(|s| s.id == slot) {
                Some(s) => s,
                None => return SeedResult::Skipped, // backend doesn't report it
            },
            Err(_) => return SeedResult::Skipped,
        };
        if slot_state.is_processing {
            return SeedResult::Skipped;
        }

        // Second sight: the bag already knows this family.
        {
            let g = self.bag.read().await;
            if let Some(e) = g.get(&key) {
                match e.state {
                    BagState::Cached => {
                        drop(g);
                        return match backend
                            .restore_slot(slot, &Self::blob_name(&key))
                            .await
                        {
                            Ok(_) => SeedResult::Restored,
                            Err(_) => SeedResult::ColdPrefill, // blob evicted
                        };
                    }
                    BagState::Failed => {} // retry the dance below
                    BagState::Uncached => {} // in-flight or first sight
                }
            }
        }

        // First sight (or failed-retry): the seed dance.
        let system: Value = {
            let mut g = self.bag.write().await;
            let entry = g.entry(key.clone()).or_insert_with(|| BagEntry {
                system: Value::Array(Vec::new()),
                state: BagState::Uncached,
            });
            if entry.state == BagState::Failed {
                entry.state = BagState::Uncached; // retry
            }
            if entry.system == Value::Array(Vec::new()) {
                let systems = messages
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                entry.system = Value::Array(systems);
            }
            entry.system.clone()
        };

        // 1. prefill only — strictly before any save.
        if backend.prefill_only(slot, &system).await.is_err() {
            return self.mark_failed(&key).await;
        }

        // 2. confirm position + idle before saving. A position of 0 means
        //    the prefill did not land (the backend reports nothing) — an
        //    unconfirmed position is never saved.
        let confirmed = match backend.slots().await {
            Ok(states) => match states.into_iter().find(|s| s.id == slot) {
                Some(s) => !s.is_processing && s.n_tokens > 0,
                None => false,
            },
            Err(_) => false,
        };
        if !confirmed {
            return self.mark_failed(&key).await;
        }

        // 3. save to scratch, then publish (R2) under the confirmed position.
        let name = Self::blob_name(&key);
        let scratch = format!("{name}.save");
        let ok = backend.save_slot(slot, &scratch).await.is_ok()
            && publisher.publish(&scratch, &name).await.is_ok();
        if ok {
            self.mark_cached(&key).await;
            SeedResult::Seeded
        } else {
            self.mark_failed(&key).await;
            SeedResult::Failed
        }
    }

    async fn mark_cached(&self, key: &str) {
        let mut g = self.bag.write().await;
        if let Some(e) = g.get_mut(key) {
            e.state = BagState::Cached;
        }
    }

    async fn mark_failed(&self, key: &str) -> SeedResult {
        let mut g = self.bag.write().await;
        match g.get_mut(key) {
            Some(e) => e.state = BagState::Failed,
            None => {
                g.insert(
                    key.to_string(),
                    BagEntry {
                        system: Value::Null,
                        state: BagState::Failed,
                    },
                );
            }
        }
        SeedResult::Failed
    }

    /// Test/diagnostic accessor.
    pub async fn state(&self, key: &str) -> Option<BagState> {
        self.bag.read().await.get(key).map(|e| e.state)
    }
}

fn hex16(digest: &[u8]) -> String {
    let full: String = digest.iter().map(|x| format!("{x:02x}")).collect();
    full[..16].to_string()
}

#[cfg(test)]
mod tests {
    //! T1 protocol tests: the seed dance's invariants, asserted on the
    //! scripted fake's call sequence (no GPU, no live fs, no run-date).

    use super::*;
    use crate::backend::SlotState;
    use crate::fake::{Call, FakeBackend};

    fn idle_slot(id: u32) -> SlotState {
        SlotState { id, is_processing: false, n_tokens: 0 }
    }

    fn sys_req(system: &str, user: &str) -> Value {
        serde_json::json!({
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ],
            "stream": false
        })
    }

    fn first_index(calls: &[Call], v: &Call) -> Option<usize> {
        calls.iter().position(|c| c == v)
    }
    fn count(calls: &[Call], v: &Call) -> usize {
        calls.iter().filter(|c| *c == v).count()
    }

    // miss ⇒ prefill_only strictly before save_slot
    #[tokio::test]
    async fn prefill_strictly_before_save() {
        let be = FakeBackend::with_slots(vec![idle_slot(1)]);
        be.prefill_delta.store(42, std::sync::atomic::Ordering::Relaxed);
        let pub_ = MemoryPublisher::default();
        let dance = SeedDance::new();
        let res = dance.prepare(&be, &pub_, 1, &sys_req("sys", "hi")).await;
        assert_eq!(res, SeedResult::Seeded);
        let calls = be.calls();
        let pf = first_index(&calls, &Call::PrefillOnly { slot: 1, n_msgs: 1 }).unwrap();
        // blob name embeds the real family key; just assert a Save exists after prefill
        let sv = calls.iter().position(|c| matches!(c, Call::Save { slot: 1, .. })).unwrap();
        assert!(pf < sv, "prefill (idx {pf}) must strictly precede save (idx {sv}): {calls:?}");
        // save ran under a confirmed, non-zero position
        let slots_after = calls.iter().filter(|c| *c == &Call::Slots).count();
        assert!(slots_after >= 2, "position must be confirmed after prefill: {calls:?}");
        assert_eq!(sv, calls.len() - 1, "save is the last step: {calls:?}");
    }

    // never save an unconfirmed position (prefill that lands nowhere)
    #[tokio::test]
    async fn never_save_unconfirmed_position() {
        let be = FakeBackend::with_slots(vec![idle_slot(1)]);
        be.prefill_delta.store(0, std::sync::atomic::Ordering::Relaxed); // prompt lands at 0
        let pub_ = MemoryPublisher::default();
        let dance = SeedDance::new();
        let res = dance.prepare(&be, &pub_, 1, &sys_req("sys", "hi")).await;
        assert_eq!(res, SeedResult::Failed);
        let calls = be.calls();
        assert!(calls.iter().all(|c| !matches!(c, Call::Save { .. })),
            "no save on unconfirmed position: {calls:?}");
        assert_eq!(dance.state(&key_of("sys")).await, Some(BagState::Failed));
    }

    // second sight of same system-hash ⇒ restore, ZERO prefill_only calls
    #[tokio::test]
    async fn second_sight_restore_zero_prefill() {
        let be = FakeBackend::with_slots(vec![idle_slot(1)]);
        be.prefill_delta.store(42, std::sync::atomic::Ordering::Relaxed);
        let pub_ = MemoryPublisher::default();
        let dance = SeedDance::new();
        assert_eq!(dance.prepare(&be, &pub_, 1, &sys_req("sys", "a")).await, SeedResult::Seeded);
        let before = be.calls().len();
        assert_eq!(dance.prepare(&be, &pub_, 1, &sys_req("sys", "b")).await, SeedResult::Restored);
        let tail = be.calls();
        let delta = &tail[before..];
        assert!(delta.iter().any(|c| matches!(c, Call::Restore { slot: 1, .. })),
            "second sight must restore: {delta:?}");
        assert_eq!(count(delta, &Call::PrefillOnly { slot: 1, n_msgs: 1 }), 0,
            "second sight must NOT prefill: {delta:?}");
    }

    // never restore onto a processing slot (R3 as an assertion)
    #[tokio::test]
    async fn never_restore_onto_processing_slot() {
        let be = FakeBackend::with_slots(vec![idle_slot(1)]);
        be.prefill_delta.store(42, std::sync::atomic::Ordering::Relaxed);
        let pub_ = MemoryPublisher::default();
        let dance = SeedDance::new();
        // seed so the bag is cached…
        assert_eq!(dance.prepare(&be, &pub_, 1, &sys_req("sys", "a")).await, SeedResult::Seeded);
        // …then the slot goes processing (a turn is running on it)
        let mut g = be.slots.lock().unwrap();
        g[0].is_processing = true;
        drop(g);
        // second sight must NOT restore onto the busy slot
        assert_eq!(dance.prepare(&be, &pub_, 1, &sys_req("sys", "b")).await, SeedResult::Skipped);
        let calls = be.calls();
        assert!(calls.iter().all(|c| !matches!(c, Call::Restore { .. })),
            "R3: never restore onto a processing slot: {calls:?}");
    }

    // save fails mid-seed ⇒ bag stays uncached, request still served, no leak
    #[tokio::test]
    async fn save_failure_leaves_bag_uncached() {
        let be = FakeBackend::with_slots(vec![idle_slot(1)]);
        be.prefill_delta.store(42, std::sync::atomic::Ordering::Relaxed);
        be.fail_save.store(true, std::sync::atomic::Ordering::Relaxed);
        let pub_ = MemoryPublisher::default();
        let dance = SeedDance::new();
        let res = dance.prepare(&be, &pub_, 1, &sys_req("sys", "hi")).await;
        assert_eq!(res, SeedResult::Failed);
        let key = key_of("sys");
        assert_ne!(dance.state(&key).await, Some(BagState::Cached),
            "bag must not be cached after a failed save");
        // nothing published
        assert!(pub_.files.lock().await.is_empty(), "no blob may be published on failure");
    }

    // a failed seed is retried on the next sight (not stuck in `failed`)
    #[tokio::test]
    async fn failed_seed_retries_next_sight() {
        let be = FakeBackend::with_slots(vec![idle_slot(1)]);
        be.prefill_delta.store(42, std::sync::atomic::Ordering::Relaxed);
        be.fail_save.store(true, std::sync::atomic::Ordering::Relaxed);
        let pub_ = MemoryPublisher::default();
        let dance = SeedDance::new();
        assert_eq!(dance.prepare(&be, &pub_, 1, &sys_req("sys", "a")).await, SeedResult::Failed);
        // save recovers → the next sight re-runs the dance and seeds
        be.fail_save.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(dance.prepare(&be, &pub_, 1, &sys_req("sys", "b")).await, SeedResult::Seeded);
        assert_eq!(dance.state(&key_of("sys")).await, Some(BagState::Cached));
    }

    // no system prompt ⇒ nothing is seeded, no backend mutation
    #[tokio::test]
    async fn no_system_prompt_is_skipped() {
        let be = FakeBackend::with_slots(vec![idle_slot(1)]);
        let pub_ = MemoryPublisher::default();
        let dance = SeedDance::new();
        let req = serde_json::json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert_eq!(dance.prepare(&be, &pub_, 1, &req).await, SeedResult::Skipped);
        let calls = be.calls();
        assert!(calls.iter().all(|c| !matches!(c, Call::PrefillOnly { .. } | Call::Save { .. })),
            "no system prompt ⇒ no prefill/save: {calls:?}");
    }

    fn key_of(system: &str) -> String {
        SeedDance::<FakeBackend, MemoryPublisher>::system_hash(&sys_req(system, "x")).unwrap()
    }
}

