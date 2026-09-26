//! The backend seam (T1): the trait the seed dance orchestrates over.
//!
//! Production is backed by [`LlamaBackend`] (reqwest to llama-server);
//! protocol tests use a scripted fake. The five methods are the whole
//! vocabulary the forest policy may use to talk to the backend — anything
//! outside this trait is a raw passthrough concern of the app layer.

use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;

/// A single llama.cpp slot as reported by `GET /slots`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotState {
    pub id: u32,
    pub is_processing: bool,
    /// The slot's current context position. On this llama.cpp build that is
    /// the slot's `n_prompt_tokens` (verified: a `max_tokens=0` prefill of a
    /// 59-token system prompt leaves the slot at `n_prompt_tokens == 59`);
    /// the T1 contract's "n_tokens" maps to it. `n_prompt_tokens_processed`
    /// is 0 on an idle slot and is NOT the position.
    pub n_tokens: u64,
}

/// A forwarded chat request's outcome: status plus the body as a byte
/// stream (SSE chunks when the client asked for a stream, the full body
/// otherwise). The stream is consumed exactly once.
pub struct ChatOutcome {
    pub status: u16,
    pub stream: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, String>> + Send>>,
}

/// The seam between oLLM's policy and the llama.cpp backend (T1).
/// Production impl: [`LlamaBackend`]; tests: a scripted fake.
pub trait Backend: Send + Sync {
    /// Per-slot `id`, `is_processing`, `n_tokens` (position).
    fn slots(&self) -> impl Future<Output = Result<Vec<SlotState>, String>> + Send;

    /// Forward a prompt with zero generation tokens (`n_predict=0`): the
    /// slot ends at the prompt's position, no tokens generated. Steered to
    /// `slot` via `id_slot` (the backend must not auto-schedule it).
    fn prefill_only(
        &self,
        slot: u32,
        messages: &Value,
    ) -> impl Future<Output = Result<Value, String>> + Send;

    /// Ask the backend to save slot `id` to scratch file `name`
    /// (`/slots/{id}?action=save`). The caller performs the fsync + rename
    /// (strict-write rule R2) via [`crate::seed::Publisher`].
    fn save_slot(&self, id: u32, name: &str) -> impl Future<Output = Result<Value, String>> + Send;

    /// Restore `name` into slot `id` (`/slots/{id}?action=restore`).
    /// Err ⇒ the blob is missing (evicted) or mismatched ⇒ cold-prefill
    /// fallback.
    fn restore_slot(
        &self,
        id: u32,
        name: &str,
    ) -> impl Future<Output = Result<Value, String>> + Send;

    /// Forward `/v1/chat/completions`; the response body is returned as a
    /// stream for pass-through.
    fn chat(&self, body: &Value) -> impl Future<Output = Result<ChatOutcome, String>> + Send;
}

/// The real backend: reqwest against a llama-server.
pub struct LlamaBackend {
    base: String,
    api_key: Option<String>,
    http: Client,
}

impl LlamaBackend {
    pub fn new(base: String, api_key: Option<String>, http: Client) -> Self {
        Self { base, api_key, http }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn decorate(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    /// `GET /props` — used for the compatibility signature at startup
    /// (an implementation detail, not part of the trait).
    pub async fn props(&self) -> Option<Value> {
        self.get_json("/props").await.ok()
    }

    async fn get_json(&self, path: &str) -> Result<Value, String> {
        let r = self
            .decorate(self.http.get(self.url(path)))
            .send()
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        r.json().await.map_err(|e| format!("GET {path} json: {e}"))
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<(u16, Value), String> {
        let r = self
            .decorate(self.http.post(self.url(path)))
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let code = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or_else(|e| json!({ "unparsed": e.to_string() }));
        Ok((code, v))
    }
}

impl Backend for LlamaBackend {
    async fn slots(&self) -> Result<Vec<SlotState>, String> {
        let v = self.get_json("/slots").await?;
        let arr = v
            .as_array()
            .ok_or_else(|| "GET /slots: not an array".to_string())?;
        let mut out = Vec::with_capacity(arr.len());
        for s in arr {
            let id = match s.get("id").and_then(|x| x.as_u64()) {
                Some(x) => x as u32,
                None => continue,
            };
            let is_processing = s
                .get("is_processing")
                .and_then(|x| x.as_bool())
                .unwrap_or(true);
            // The slot's context position on this build. `n_prompt_tokens`
            // is the source of truth (verified live); `n_tokens` is absent.
            let n_tokens = s
                .get("n_prompt_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            out.push(SlotState { id, is_processing, n_tokens });
        }
        Ok(out)
    }

    async fn prefill_only(&self, slot: u32, messages: &Value) -> Result<Value, String> {
        // `max_tokens: 0` is the chat-completions form of n_predict=0:
        // forward the prompt, generate nothing.
        let mut body = json!({
            "messages": messages,
            "stream": false,
            "id_slot": slot,
        });
        if let Some(o) = body.as_object_mut() {
            o.insert("max_tokens".into(), json!(0));
        }
        match self.post_json("/v1/chat/completions", body).await {
            Ok((code, v)) if code < 300 => Ok(v),
            Ok((code, v)) => Err(format!("prefill_only: {code}: {v}")),
            Err(e) => Err(e),
        }
    }

    async fn save_slot(&self, id: u32, name: &str) -> Result<Value, String> {
        match self
            .post_json(&format!("/slots/{id}?action=save"), json!({ "filename": name }))
            .await
        {
            Ok((code, v)) if code < 300 => Ok(v),
            Ok((code, v)) => Err(format!("save: {code}: {v}")),
            Err(e) => Err(e),
        }
    }

    async fn restore_slot(&self, id: u32, name: &str) -> Result<Value, String> {
        match self
            .post_json(&format!("/slots/{id}?action=restore"), json!({ "filename": name }))
            .await
        {
            Ok((code, v)) if code < 300 => Ok(v),
            Ok((code, v)) => Err(format!("restore: {code}: {v}")),
            Err(e) => Err(e),
        }
    }

    async fn chat(&self, body: &Value) -> Result<ChatOutcome, String> {
        let r = self
            .decorate(self.http.post(self.url("/v1/chat/completions")))
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = r.status().as_u16();
        let stream: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, String>> + Send>> =
            r.bytes_stream().map(|c| c.map_err(|e| e.to_string())).boxed();
        Ok(ChatOutcome { status, stream })
    }
}
