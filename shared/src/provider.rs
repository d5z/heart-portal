use async_trait::async_trait;
use futures_util::Stream;
use crate::{ChatRequest, ChatResponse, StreamEvent};
use std::pin::Pin;

/// Provider trait for different LLM backends
#[async_trait]
pub trait Provider: Send + Sync {
    /// Get provider name
    fn name(&self) -> &str;

    /// Single-turn chat with messages
    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse>;

    /// Streaming chat — returns a stream of events
    async fn chat_stream(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>>>;

    /// Simple convenience method for basic chat
    async fn simple_chat(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        use crate::ChatMessage;
        
        let mut messages = Vec::new();
        
        if let Some(system) = system_prompt {
            messages.push(ChatMessage::system(system));
        }
        messages.push(ChatMessage::user(message));

        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };

        let response = self.chat(request, model, temperature).await?;
        Ok(response.text_or_empty().to_string())
    }
}
