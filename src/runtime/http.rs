#![allow(dead_code)]

use std::collections::HashMap;

use crate::{error::KuResult, value::Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct HandlerId(pub(crate) usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackpressureConfig {
    pub max_connections: usize,
    pub max_active_requests: usize,
    pub max_pending_requests: usize,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            max_active_requests: 256,
            max_pending_requests: 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceLimits {
    pub read_header_timeout_ms: u64,
    pub read_body_timeout_ms: u64,
    pub write_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub handler_timeout_ms: u64,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub backpressure: BackpressureConfig,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            read_header_timeout_ms: 5_000,
            read_body_timeout_ms: 10_000,
            write_timeout_ms: 10_000,
            idle_timeout_ms: 5_000,
            handler_timeout_ms: 15_000,
            max_header_bytes: 16 * 1024,
            max_body_bytes: 1_000_000,
            backpressure: BackpressureConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutePattern {
    pub method: String,
    pub path: String,
    pub shape: String,
    pub param_names: Vec<String>,
    pub handler: HandlerId,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RouterTrie {
    pub by_method: HashMap<String, HashMap<String, RoutePattern>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KuHttpRequest {
    pub method: String,
    pub path: String,
    pub params: HashMap<String, String>,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KuHttpResponse {
    pub status: i64,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl KuHttpResponse {
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let Value::Object(fields) = value else {
            return None;
        };
        let status = match fields.get("status") {
            Some(Value::Int(status)) => *status,
            _ => return None,
        };
        let headers = match fields.get("headers") {
            Some(Value::Object(headers)) => headers
                .iter()
                .filter_map(|(name, value)| match value {
                    Value::String(value) => Some((name.clone(), value.clone())),
                    _ => None,
                })
                .collect(),
            _ => return None,
        };
        let body = match fields.get("body") {
            Some(Value::String(body)) => body.clone(),
            _ => return None,
        };
        Some(Self {
            status,
            headers,
            body,
        })
    }
}

pub(crate) trait HttpDriver {
    type Listener;

    fn bind(
        &self,
        address: &str,
        router: RouterTrie,
        limits: ResourceLimits,
    ) -> KuResult<Self::Listener>;
    fn run(&self, listener: Self::Listener) -> KuResult<()>;
}

pub(crate) struct HandlerCall<'a> {
    pub handler: HandlerId,
    pub request: KuHttpRequest,
    pub response_slot: &'a mut Option<KuHttpResponse>,
}
