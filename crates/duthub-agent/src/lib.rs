//! UI-free Agent runtime support: LLM provider probes and model base-URL
//! validation.

#![forbid(unsafe_code)]

pub mod llm;

pub use llm::{
    LlmError, TestRequest, TestResponse, fetch_available_models, test_model_api,
    test_model_connection, validate_model_base,
};
