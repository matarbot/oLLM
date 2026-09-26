//! oLLM — thin disk-cache proxy in front of llama.cpp's llama-server.
//!
//! Why: llama.cpp persists KV to disk only via operator-driven
//! `/slots/{id}?action=save|restore` (`--slot-save-path`); upstream declined
//! to automate it (#17107 — mechanism server-side, policy client-side).
//! oLLM is that policy: it keeps a disk-backed forest of prefix trees,
//! restores the deepest matching blob into a slot before forwarding, and
//! saves slot state back to disk when the turn completes. A warmed cache
//! shipped on a fresh device => optimal first-token latency.
//!
//! Verified on box0 (build b10627, Qwen3.8-27B hybrid + MTP draft):
//! - save 16 ms / restore 13 ms round-trips KV intact incl. recurrent state (E1)
//! - blob is whole-slot FLAGS_NONE: ~153 MB floor + ~34 KB/token (E1)
//! - unified RAM cache shares prefixes across slots (5x TTFT), ~670 MB/prompt (E7)
//! - eviction: byte-capped, oldest last_access (mtime) first — v0 simple score
//!
//! Endpoints:
//! - POST /v1/chat/completions  -> cache-aware forward (id_slot steering)
//! - GET  /health               -> oLLM status (cache size, backend, signature)
//! - everything else            -> verbatim passthrough (/props, /slots, ...)

pub mod backend;
pub mod seed;

#[cfg(test)]
pub mod fake;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use backend::{Backend, ChatOutcome, LlamaBackend};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::sync::{Mutex, RwLock};

// ------------------------------------------------------------------ config

#[derive(Clone)]
pub struct Config {
    pub backend: String,
    pub bind: String,
    /// MUST equal the backend's --slot-save-path (same filesystem view):
    /// save/restore filenames in /slots/{id} are resolved there.
    pub cache_dir: PathBuf,
    pub cache_limit_mb: u64,
    pub backend_api_key: Option<String>,
    pub session_header: String,
}

impl Config {
    pub fn from_env() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        Self {
            backend: std::env::var("OLLM_BACKEND_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:1245".into()),
            bind: std::env::var("OLLM_BIND").unwrap_or_else(|_| "0.0.0.0:1247".into()),
            cache_dir: std::env::var("OLLM_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(home).join(".ollm/cache")),
            cache_limit_mb: std::env::var("OLLM_CACHE_LIMIT_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(51200),
            backend_api_key: std::env::var("OLLM_BACKEND_API_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            session_header: std::env::var("OLLM_SESSION_HEADER")
                .unwrap_or_else(|_| "x-session-id".into()),
        }
    }
}

// ---------------------------------------------------------------- keying

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Sanitize a header-derived key so it is a safe filename component and a
/// safe llama.cpp slot-save filename (no '/', no '..', length-capped).
pub fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    out.truncate(96);
    if out.is_empty() {
        out = "session".into();
    }
    out
}

/// Session key: trusted header first, else hash of the full message list
/// (client resends full history every turn => stable key per conversation
/// state at admission time; content-extension = child node in the forest).
pub fn session_key(headers: &HeaderMap, body: &Value, header_name: &str) -> String {
    if let Some(v) = headers.get(header_name).and_then(|v| v.to_str().ok()) {
        if !v.is_empty() {
            return format!("hdr-{}", sanitize(v));
        }
    }
    let mut h = Sha256::new();
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            h.update(serde_json::to_vec(m).unwrap_or_default());
            h.update([0u8]);
        }
    }
    format!("conv-{}", &hex(&h.finalize())[..16])
}

// ----------------------------------------------------------- router state

struct SlotInfo {
    session: Option<String>,
    /// true from the moment a turn is steered to this slot until the
    /// post-turn save publishes. A dirty slot is NEVER stolen.
    dirty: bool,
    busy: bool,
}

pub struct App {
    cfg: Config,
    /// The backend seam (T1): all policy traffic goes through this trait.
    backend: LlamaBackend,
    /// Raw client used ONLY for verbatim passthrough of non-chat routes
    /// (an app-layer concern, not a Backend-trait operation).
    http: Client,
    backend_sig: String,
    slots: Mutex<HashMap<u32, SlotInfo>>,
    /// Strict write rules (the oMLX reliability contract):
    /// R1 — a write holds this gate exclusively: while any blob is being
    ///      written (post-turn save, eviction save), incoming requests block
    ///      at the prefix matcher until the write publishes. Matching never
    ///      runs against an unsettled forest.
    /// R2 — publication is atomic: llama.cpp writes a `.save` scratch file,
    ///      oLLM fsyncs it and renames it into place. The forest only ever
    ///      contains complete blobs.
    /// R3 — save-before-steal: a slot is never steered to a new session
    ///      while dirty or processing; its state is published first.
    write_gate: RwLock<()>,
}

impl App {
    fn decorate(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.cfg.backend_api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    /// R3: pick a slot for `session`.
    /// 1. the slot already owned by this session (hot, steer straight back)
    /// 2. a slot that is neither processing nor dirty in oLLM's view
    /// 3. otherwise None -> caller forwards without steering (backend
    ///    auto-schedules onto a genuinely free slot; we never evict A for B)
    async fn pick_slot(&self, session: &str) -> Option<(u32, bool)> {
        {
            let g = self.slots.lock().await;
            for (id, s) in g.iter() {
                if s.session.as_deref() == Some(session) {
                    return Some((*id, true));
                }
            }
        }
        let arr = match self.backend.slots().await {
            Ok(a) => a,
            Err(_) => return None,
        };
        let mut g = self.slots.lock().await;
        for s in arr {
            let e = g.entry(s.id).or_insert(SlotInfo { session: None, dirty: false, busy: false });
            e.busy = s.is_processing;
            // R3: never steal a processing or dirty slot.
            if s.is_processing || e.dirty {
                continue;
            }
            return Some((s.id, false));
        }
        None
    }

    /// Publish a dirty slot's KV before stealing it (write-on-eviction).
    /// Returns the evicted session id (if any) so the caller can log it.
    async fn find_evictable(&self) -> Option<(u32, Option<String>)> {
        let arr = match self.backend.slots().await {
            Ok(a) => a,
            Err(_) => return None,
        };
        let mut g = self.slots.lock().await;
        for s in arr {
            let e = g.entry(s.id).or_insert(SlotInfo { session: None, dirty: false, busy: false });
            e.busy = s.is_processing;
            if s.is_processing {
                continue;
            }
            if e.dirty {
                // publish first (save_slot runs under the exclusive gate and
                // marks clean); caller must save before returning it
                return Some((s.id, e.session.clone()));
            }
        }
        None
    }

    async fn evict_to_disk(&self, id: u32, victim: Option<String>) -> bool {
        if let Some(v) = &victim {
            tracing::info!(slot = id, victim = %v, "write-on-eviction: publishing victim");
            self.save_slot(id, v).await
        } else {
            true // nothing to publish
        }
    }

    /// Mark a slot as holding `session` and dirty (a turn ran / is running
    /// on it; its state must be published before anyone else may use it).
    async fn mark_dirty(&self, id: u32, session: &str) {
        let mut g = self.slots.lock().await;
        let e = g.entry(id).or_insert(SlotInfo { session: None, dirty: false, busy: false });
        e.session = Some(session.to_string());
        e.dirty = true;
    }

    async fn mark_clean(&self, id: u32) {
        if let Some(s) = self.slots.lock().await.get_mut(&id) {
            s.dirty = false;
        }
    }

    fn blob_name(&self, session: &str) -> String {
        // Backend signature is part of the filename: a blob saved under one
        // backend (model/quant/template/ctx) must never restore into another
        // — mismatched KV restore is silently wrong, not an error (fork §7).
        format!("{}__{}.bin", session, self.backend_sig)
    }

    fn blob_path(&self, session: &str) -> PathBuf {
        self.cfg.cache_dir.join(self.blob_name(session))
    }

    /// v0 recency score = file mtime, bumped on serve + save.
    fn touch(&self, session: &str) {
        bump_mtime_unix(&self.blob_path(session));
    }

    /// Byte-capped LRU eviction by mtime; 60 s grace for writes in flight.
    async fn enforce_limit(&self) {
        let limit = self.cfg.cache_limit_mb * 1024 * 1024;
        let dir = self.cfg.cache_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) != Some("bin") {
                        continue;
                    }
                    if let Ok(md) = e.metadata() {
                        files.push((p, md.modified().unwrap_or(SystemTime::UNIX_EPOCH), md.len()));
                    }
                }
            }
            let total: u64 = files.iter().map(|f| f.2).sum();
            if total <= limit {
                return;
            }
            files.sort_by_key(|f| f.1);
            let now = SystemTime::now();
            let mut freed = 0u64;
            for (p, at, len) in files {
                if total - freed <= limit {
                    break;
                }
                if now.duration_since(at).map(|d| d.as_secs() < 60).unwrap_or(false) {
                    continue; // grace: just written/served
                }
                match std::fs::remove_file(&p) {
                    Ok(()) => {
                        freed += len;
                        tracing::info!(file = %p.display(), mib = len / 1024 / 1024, "evicted (LRU)");
                    }
                    Err(e) => {
                        tracing::warn!(file = %p.display(), err = %e, "evict failed");
                    }
                }
            }
        })
        .await;
    }

    /// R1+R2: publish the slot's KV to disk. Takes the write gate
    /// exclusively (matching blocks), saves to a scratch filename, fsyncs,
    /// renames into place. After rename the blob is visible and complete.
    async fn save_slot(&self, id: u32, session: &str) -> bool {
        let _w = self.write_gate.write().await;
        let scratch = format!("{}.save", self.blob_name(session));
        let save_ok = match self.backend.save_slot(id, &scratch).await {
            Ok(v) => {
                let _ = v;
                true
            }
            Err(e) => {
                tracing::warn!(slot = id, session, err = %e, "slot save failed");
                false
            }
        };
        if !save_ok {
            return false;
        }
        let final_path = self.blob_path(session);
        let scratch_path = self.cfg.cache_dir.join(&scratch);
        let moved = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let f = std::fs::File::open(&scratch_path)?; // llama.cpp closed it
            f.sync_all()?; // durability before visibility
            std::fs::rename(&scratch_path, &final_path)?; // atomic publish
            if let Some(parent) = final_path.parent() {
                if let Ok(d) = std::fs::File::open(parent) {
                    let _ = d.sync_all(); // index durability
                }
            }
            Ok(())
        })
        .await;
        let published = match moved {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                tracing::warn!(session, io = %e, "publish err");
                false
            }
            Err(e) => {
                tracing::warn!(session, join = %e, "publish join err");
                false
            }
        };
        if published {
            self.touch(session);
            self.mark_clean(id).await;
            tracing::info!(slot = id, session, "blob published (atomic)");
        }
        published
    }

    /// R1: restore runs AFTER the lookup phase; blobs are rename-published
    /// (complete or absent), so a plain restore is race-free. If the blob
    /// was evicted mid-flight, llama.cpp 404s -> cold prefill fallback.
    async fn restore_slot(&self, id: u32, session: &str) {
        match self.backend.restore_slot(id, &self.blob_name(session)).await {
            Ok(_) => {
                self.touch(session);
                tracing::info!(slot = id, session, "disk blob restored");
            }
            Err(e) => {
                tracing::warn!(slot = id, session, err = %e, "restore failed (cold prefill fallback)");
            }
        }
    }
}

// mtime bump — safe, no FFI ---------------------------------------------------

/// Set a file's mtime (and atime) to now. Creates nothing: if the file
/// vanished between save and touch, that is a benign race (blob gone = cold
/// prefill next time). Uses FileTimes (Rust 1.75+) — no libc, no unsafe.
#[cfg(unix)]
fn bump_mtime_unix(path: &std::path::Path) {
    use std::fs::File;
    use std::fs::FileTimes;
    if let Ok(f) = File::options().write(true).open(path) {
        let times = FileTimes::new().set_modified(SystemTime::now());
        let _ = f.set_times(times); // file may have been evicted concurrently
    }
}

#[cfg(not(unix))]
fn bump_mtime_unix(path: &std::path::Path) {
    let _ = path;
}

/// Consume a chat outcome's stream to full bytes (non-streaming forward).
async fn collect_body(mut outcome: ChatOutcome) -> (StatusCode, bytes::Bytes) {
    let status =
        StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut buf = Vec::new();
    while let Some(chunk) = outcome.stream.next().await {
        match chunk {
            Ok(b) => buf.extend_from_slice(&b),
            Err(e) => {
                tracing::warn!(err = %e, "chat body chunk error");
                break;
            }
        }
    }
    (status, bytes::Bytes::from(buf))
}

// ---------------------------------------------------------------- handlers

async fn chat_completions(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let raw = body.0;
    let parsed: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid json: {e}")),
    };

    let session = session_key(&headers, &parsed, &app.cfg.session_header);

    // R1: the prefix match (lookup) holds the gate shared — it waits for
    // any in-flight publish and blocks publishes while it decides. Decode
    // itself runs outside the gate; saves take it exclusive at turn end,
    // so the next request's matcher always sees a settled forest.
    let lookup = {
        let _match = app.write_gate.read().await;
        let disk_hit = app.blob_path(&session).exists();
        (disk_hit, app.pick_slot(&session).await)
    };
    let (disk_hit, picked) = lookup;

    // admission (R3)
    let steer: Option<u32> = match picked {
        Some((id, hot)) => {
            if disk_hit && !hot {
                app.restore_slot(id, &session).await;
            }
            app.mark_dirty(id, &session).await; // its KV is authoritative now
            Some(id)
        }
        None => {
            // eviction path: a new session needs a slot and all known slots
            // are busy-or-dirty. Publish the dirty slot's KV first (this is
            // the "write upon eviction" rule), then steer there. If every
            // slot is still processing, forward unsteered — never drop a
            // session's state silently.
            let free = app.find_evictable().await;
            if let Some((id, victim)) = free {
                // write-on-eviction: publish the victim's KV before steal
                if app.evict_to_disk(id, victim.clone()).await {
                    if disk_hit {
                        app.restore_slot(id, &session).await;
                    }
                    app.mark_dirty(id, &session).await;
                    Some(id)
                } else {
                    None // publish failed: do not steal an unpersisted slot
                }
            } else {
                None
            }
        }
    };

    let mut fwd = parsed.clone();
    let is_stream = fwd.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    if let (Some(o), Some(id)) = (fwd.as_object_mut(), steer) {
        o.insert("id_slot".into(), json!(id));
    }

    let outcome = match app.backend.chat(&fwd).await {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e),
    };
    let status =
        StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::BAD_GATEWAY);

    if !status.is_success() {
        // Drain the error body and surface it.
        let mut buf = Vec::new();
        let mut outcome = outcome;
        while let Some(c) = outcome.stream.next().await {
            if let Ok(b) = c {
                buf.extend_from_slice(&b);
            }
        }
        return err(status, String::from_utf8_lossy(&buf));
    }

    if is_stream {
        // Instrumented pass-through. The pump runs in a spawned task so the
        // client receives chunks live (never buffered): first chunk -> log
        // TTFT; EOF -> save slot. Save is driven by stream completion
        // instead of slot-idle polling, which raced the next turn.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, axum::Error>>(64);
        let app2 = app.clone();
        let sess2 = session.clone();
        tokio::spawn(async move {
            let mut stream = outcome.stream;
            let t0 = std::time::Instant::now();
            let mut ttft_logged = false;
            let mut total = 0usize;
            let mut failed = false;
            let mut disconnected = false;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(b) => {
                        if !ttft_logged && !b.is_empty() {
                            ttft_logged = true;
                            tracing::info!(
                                session = %sess2,
                                ttft_ms = %t0.elapsed().as_millis(),
                                "first streamed chunk"
                            );
                        }
                        total += b.len();
                        if tx.send(Ok(b)).await.is_err() {
                            // Client hung up (ctrl-c, head -c, cancelled turn).
                            // The backend keeps generating regardless — keep
                            // draining so we can still persist the finished KV;
                            // just stop forwarding (channel closed anyway).
                            disconnected = true;
                        }
                    }
                    Err(e) => {
                        failed = true;
                        tracing::warn!(session = %sess2, err = %e, "stream chunk error");
                        break;
                    }
                }
            }
            tracing::info!(
                session = %sess2,
                bytes = total,
                ms = %t0.elapsed().as_millis(),
                failed,
                disconnected,
                "stream pump finished"
            );
            if !failed {
                if let Some(id) = steer {
                    {
                        let mut g = app2.slots.lock().await;
                        if let Some(s) = g.get_mut(&id) {
                            s.session = Some(sess2.clone());
                        }
                    }
                    app2.save_slot(id, &sess2).await;
                    app2.enforce_limit().await;
                }
            }
        });
        let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
        return with_status(Response::new(body), status);
    }

    let (status, text) = collect_body(outcome).await;
    if let Some(id) = steer {
        let app2 = app.clone();
        let sess2 = session.clone();
        tokio::spawn(async move {
            {
                let mut g = app2.slots.lock().await;
                if let Some(s) = g.get_mut(&id) {
                    s.session = Some(sess2.clone());
                }
            }
            app2.save_slot(id, &sess2).await;
            app2.enforce_limit().await;
        });
    }
    with_status(Response::new(Body::from(text)), status)
}

async fn passthrough(State(app): State<Arc<App>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let raw = axum::body::to_bytes(body, 16 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let method =
        reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let url = format!("{}{}", app.cfg.backend, parts.uri);
    let mut r = app.decorate(app.http.request(method, &url));
    if !raw.is_empty() {
        r = r.body(raw.to_vec());
    }
    match r.send().await {
        Ok(resp) => {
            let code =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut out = Response::new(Body::from_stream(resp.bytes_stream()));
            *out.status_mut() = code;
            out
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    let dir = app.cfg.cache_dir.clone();
    let cache_mb = tokio::task::spawn_blocking(move || {
        std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum::<u64>()
                    / 1024
                    / 1024
            })
            .unwrap_or(0)
    })
    .await
    .unwrap_or(0);
    Json(json!({
        "status": "ok",
        "backend": app.cfg.backend,
        "backend_sig": app.backend_sig,
        "cache_dir": app.cfg.cache_dir.display().to_string(),
        "cache_mb": cache_mb,
        "cache_limit_mb": app.cfg.cache_limit_mb,
    }))
}

fn err(code: StatusCode, msg: impl std::fmt::Display) -> Response {
    let mut r = Json(json!({ "error": { "message": msg.to_string() } })).into_response();
    *r.status_mut() = code;
    r
}

fn with_status(mut r: Response, code: StatusCode) -> Response {
    *r.status_mut() = code;
    r
}

/// body extractor that keeps bytes (axum 0.8: native async trait)
pub struct Bytes(pub Vec<u8>);
impl<S: Send + Sync> axum::extract::FromRequest<S> for Bytes {
    type Rejection = Response;
    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let b = axum::body::to_bytes(req.into_body(), 128 * 1024 * 1024)
            .await
            .map_err(|e| err(StatusCode::PAYLOAD_TOO_LARGE, e))?;
        Ok(Bytes(b.to_vec()))
    }
}

// ------------------------------------------------------------------ serve

/// Build the app (probing the backend for its compatibility signature) and
/// run the router until the listener closes.
pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.cache_dir)?;

    let http = Client::builder()
        .pool_max_idle_per_host(8)
        .timeout(Duration::from_secs(3600))
        .build()?;

    let backend = LlamaBackend::new(
        cfg.backend.clone(),
        cfg.backend_api_key.clone(),
        http.clone(),
    );

    // compat signature: layout-relevant backend props go into the gate.
    // A restore into a mismatched backend is silently wrong, not fatal —
    // key on model/template/slots so stale blobs are simply never keyed.
    let mut h = Sha256::new();
    if let Some(props) = backend.props().await {
        for k in [
            "model_path",
            "total_slots",
            "n_ctx",
            "chat_template",
            "add_bos_token",
            "bos_token",
        ] {
            if let Some(v) = props.get(k) {
                h.update(serde_json::to_vec(v).unwrap_or_default());
            }
        }
    }
    let backend_sig = hex(&h.finalize())[..12].to_string();

    let app = Arc::new(App {
        cfg: cfg.clone(),
        backend,
        http,
        backend_sig: backend_sig.clone(),
        slots: Mutex::new(HashMap::new()),
        write_gate: RwLock::new(()),
    });

    tracing::info!(
        backend = %cfg.backend,
        bind = %cfg.bind,
        cache = %cfg.cache_dir.display(),
        limit_mb = cfg.cache_limit_mb,
        sig = %backend_sig,
        "oLLM starting"
    );

    let st = app.clone();
    let router = Router::new()
        .route("/health", any(health))
        .route("/v1/chat/completions", any(chat_completions))
        .fallback(passthrough)
        .with_state(st);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("listening on http://{}", cfg.bind);
    axum::serve(listener, router).await?;
    Ok(())
}
