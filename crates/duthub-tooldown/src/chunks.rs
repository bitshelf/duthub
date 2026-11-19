//! Chunk-source abstraction: the install pipeline is driven by any body source, so it is
//! fully testable without network access.

use crate::error::ToolInstallError;

/// Source of body chunks for the download loop.
pub(crate) trait ChunkSource {
    /// Next body chunk; `None` marks end of stream.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ToolInstallError>;

    /// Total payload size when the source declares one, `None` otherwise.
    /// Only a real server-reported length may flow through here — progress
    /// reporting must never invent a total.
    fn total_bytes(&self) -> Option<u64> {
        None
    }
}

/// Live HTTP response body (the production source).
pub(crate) struct HttpSource {
    response: reqwest::Response,
    url: String,
}

impl HttpSource {
    pub(crate) fn new(response: reqwest::Response, url: impl Into<String>) -> Self {
        Self {
            response,
            url: url.into(),
        }
    }
}

impl ChunkSource for HttpSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ToolInstallError> {
        let url = self.url.clone();
        // `Response::chunk` is always available (no `stream` feature needed); chunks are
        // copied into owned buffers to keep the trait network-independent.
        match self.response.chunk().await {
            Ok(Some(bytes)) => Ok(Some(bytes.to_vec())),
            Ok(None) => Ok(None),
            Err(source) => Err(ToolInstallError::Http { url, source }),
        }
    }

    fn total_bytes(&self) -> Option<u64> {
        self.response.content_length()
    }
}
