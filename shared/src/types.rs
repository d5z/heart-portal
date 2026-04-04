use serde::{Deserialize, Serialize};

/// An image content block attached to a message (for multimodal LLM input).
///
/// Stored separately from text content — each adapter converts to its own wire format:
/// - Anthropic: `{ "type": "image", "source": { "type": "base64", ... } }`
/// - OpenAI/Codex: `{ "type": "image_url", "image_url": { "url": "data:...;base64,..." } }`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageContent {
    /// MIME type: "image/png", "image/jpeg", "image/gif", "image/webp"
    pub media_type: String,
    /// Base64-encoded image data
    pub data: String,
}

/// Maximum base64 payload size (20 MB).
const IMAGE_MAX_BASE64_BYTES: usize = 20 * 1024 * 1024;

/// Supported MIME types for image input.
const SUPPORTED_IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

impl ImageContent {
    /// Create an ImageContent from base64 data with magic-byte MIME sniffing.
    ///
    /// Returns `Err` if:
    /// - `data` is empty
    /// - `data` exceeds 20 MB
    /// - Magic bytes don't match any supported image format
    pub fn from_base64(data: String) -> Result<Self, String> {
        if data.is_empty() {
            return Err("empty image payload".into());
        }
        if data.len() > IMAGE_MAX_BASE64_BYTES {
            return Err(format!(
                "image too large: {} bytes (max {})",
                data.len(),
                IMAGE_MAX_BASE64_BYTES
            ));
        }
        let media_type = sniff_mime_from_base64(&data)?;
        Ok(Self { media_type, data })
    }

    /// Create from raw bytes: encodes to base64 and sniffs MIME.
    pub fn from_bytes(raw: &[u8]) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("empty image payload".into());
        }
        let media_type = sniff_mime_from_bytes(raw)?;
        let data = base64_encode(raw);
        if data.len() > IMAGE_MAX_BASE64_BYTES {
            return Err(format!(
                "image too large: {} base64 bytes (max {})",
                data.len(),
                IMAGE_MAX_BASE64_BYTES
            ));
        }
        Ok(Self { media_type, data })
    }

    /// Validate that this ImageContent has a supported MIME type.
    pub fn is_supported(&self) -> bool {
        SUPPORTED_IMAGE_TYPES.contains(&self.media_type.as_str())
    }
}

/// Sniff MIME type from first bytes of base64-encoded data.
fn sniff_mime_from_base64(b64: &str) -> Result<String, String> {
    // Decode enough for magic bytes (16 bytes → ~24 base64 chars)
    let prefix = if b64.len() > 24 { &b64[..24] } else { b64 };
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(prefix)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(prefix))
        .map_err(|e| format!("invalid base64: {e}"))?;
    sniff_mime_from_bytes(&bytes)
}

/// Sniff MIME type from raw bytes (magic-byte detection).
fn sniff_mime_from_bytes(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        return Ok("image/jpeg".into());
    }
    if bytes.len() >= 4 && bytes[..4] == [0x89, 0x50, 0x4E, 0x47] {
        return Ok("image/png".into());
    }
    if bytes.len() >= 3 && bytes[..3] == [0x47, 0x49, 0x46] {
        return Ok("image/gif".into());
    }
    if bytes.len() >= 12
        && bytes[..4] == [0x52, 0x49, 0x46, 0x46]
        && bytes[8..12] == [0x57, 0x45, 0x42, 0x50]
    {
        return Ok("image/webp".into());
    }
    Err("unrecognized image format (magic bytes don't match png/jpeg/gif/webp)".into())
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A single message in a conversation (OpenAI-compatible format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Reasoning/thinking content from extended thinking models.
    /// OpenRouter returns this as a separate field (not in content blocks).
    /// Stored and passed through unchanged — API ignores historical thinking (no token cost).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Anthropic thinking block signature (required for passing back thinking in history).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
    /// Image content blocks for multimodal input.
    /// Populated by surface builder for current-turn images; adapters convert to wire format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageContent>>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            reasoning_signature: None,
            images: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            reasoning_signature: None,
            images: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            reasoning_signature: None,
            images: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            name: Some(name.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            reasoning_signature: None,
            images: None,
        }
    }

    /// Get content as str, or empty string if None
    pub fn content_str(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}

/// A tool call requested by the LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// Gemini thought_signature — opaque base64 blob required for thinking + tool_use.
    /// Must be passed back in subsequent assistant messages for the API to process tool results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// Token usage statistics from the API response
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

/// Response from the LLM provider
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// Text content of the response
    pub text: Option<String>,
    /// Tool calls requested by the LLM
    pub tool_calls: Vec<ToolCall>,
    /// Token usage information
    pub usage: Option<TokenUsage>,
    /// Raw reasoning content from thinking models
    pub reasoning_content: Option<String>,
    /// Anthropic thinking block signature (required for history playback)
    pub reasoning_signature: Option<String>,
}

impl ChatResponse {
    /// Check if the response has tool calls
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Get text content or empty string
    pub fn text_or_empty(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }
}

/// Request structure for chat calls
#[derive(Debug, Clone)]
pub struct ChatRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [serde_json::Value]>, // Simplified for Phase 1
}

/// Result of a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    pub success: bool,
    pub is_error: bool,
}

impl ToolResult {
    /// Create a successful result
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            success: true,
            is_error: false,
        }
    }

    /// Create a failed result
    pub fn error(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            success: false,
            is_error: true,
        }
    }

    /// Create a failed result (alias for runtime compatibility)
    pub fn err(output: impl Into<String>) -> Self {
        Self::error(output)
    }
}

/// A streaming event from the LLM provider
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Text content delta
    Delta(String),
    /// Reasoning/thinking content delta (extended thinking models)
    ReasoningDelta(String),
    /// Anthropic thinking block signature (emitted once after all thinking deltas)
    ReasoningSignature(String),
    /// A complete tool call (accumulated from fragments)
    ToolCall(ToolCallInfo),
    /// Tool execution result
    ToolResult { name: String, preview: String },
    /// Starting a new thinking round
    Thinking(usize),
    /// Stream finished
    Done,
    /// Error occurred during streaming
    Error(String),
}

/// Information about a tool call (used in streaming)
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// Gemini thought_signature for thinking + tool_use
    pub thought_signature: Option<String>,
}

/// Specification of a tool for LLM consumption
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Which hint layer to update.
/// V3: only Mid is active. Far/Near removed as dead variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HintLayer {
    /// Mid-field: association pool state, current attention shape
    Mid,
}

/// Simplified message for LLM IPC requests (cron → core).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<LlmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Tool call in LLM IPC format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Response data from an LLM IPC call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponseData {
    pub text: Option<String>,
    pub tool_calls: Vec<LlmToolCall>,
}

/// A single card produced by 小反刍 (RS v1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardData {
    /// Topic tags (1-3 keywords)
    pub topic: Vec<String>,
    /// One-line summary (~50 chars)
    pub summary: String,
    /// Importance: 1=🥉 2=🥈 3=🥇 4=💎
    pub importance: u8,
}

// SanitizableMessage implementation for ChatMessage
impl crate::sanitize::SanitizableMessage for ChatMessage {
    fn role(&self) -> &str { &self.role }

    fn tool_call_ids(&self) -> Vec<String> {
        self.tool_calls.as_ref().map(|calls| {
            calls.iter().filter_map(|c| {
                c.get("id").and_then(|v| v.as_str()).map(String::from)
            }).collect()
        }).unwrap_or_default()
    }

    fn tool_result_id(&self) -> Option<String> {
        self.tool_call_id.clone()
    }

    fn has_tool_calls(&self) -> bool {
        self.tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false)
    }

    fn has_content(&self) -> bool {
        let has_text = self.content.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
        let has_reasoning = self
            .reasoning_content
            .as_ref()
            .map(|c| !c.is_empty())
            .unwrap_or(false);
        has_text || has_reasoning
    }

    fn merge_tool_calls(&mut self, other: &Self) {
        if let Some(ref other_calls) = other.tool_calls {
            self.tool_calls.get_or_insert_with(Vec::new).extend(other_calls.iter().cloned());
        }
    }

    fn merge_content(&mut self, other: &Self) {
        if let Some(ref new_content) = other.content {
            if !new_content.is_empty() {
                if let Some(ref mut existing) = self.content {
                    existing.push('\n');
                    existing.push_str(new_content);
                } else {
                    self.content = other.content.clone();
                }
            }
        }
    }

    fn strip_tool_calls(&mut self, orphan_ids: &std::collections::HashSet<String>) -> usize {
        if let Some(ref mut calls) = self.tool_calls {
            calls.retain(|c| {
                c.get("id").and_then(|v| v.as_str())
                    .map(|id| !orphan_ids.contains(id))
                    .unwrap_or(true)
            });
            calls.len()
        } else {
            0
        }
    }

    fn clear_tool_calls(&mut self) {
        self.tool_calls = None;
    }

    fn synthetic_assistant(text: &str) -> Self {
        ChatMessage::assistant(text)
    }
}
