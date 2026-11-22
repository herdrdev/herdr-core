//! JSON-RPC 2.0 envelopes, errors with stable kinds, and the method table macro.

use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub kind: String,
    pub message: String,
    pub hint: String,
}

impl RpcError {
    pub fn new(kind: &str, message: impl Into<String>, hint: impl Into<String>) -> Self {
        RpcError { code: -32000, kind: kind.into(), message: message.into(), hint: hint.into() }
    }
    fn protocol(code: i64, kind: &str, message: impl Into<String>) -> Self {
        RpcError { code, kind: kind.into(), message: message.into(), hint: "see `hc schema`".into() }
    }
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::protocol(-32700, "parse_error", message)
    }
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::protocol(-32600, "invalid_request", message)
    }
    pub fn unknown_method(method: &str) -> Self {
        Self::protocol(-32601, "unknown_method", format!("unknown method {method:?}"))
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::protocol(-32602, "invalid_params", message)
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }
    pub fn internal(message: impl Into<String>) -> Self {
        RpcError { code: -32603, kind: "internal".into(), message: message.into(), hint: String::new() }
    }

    pub fn to_json(&self) -> Value {
        json!({"code": self.code, "message": self.message, "data": {"kind": self.kind, "hint": self.hint}})
    }

    pub fn from_json(v: &Value) -> RpcError {
        RpcError {
            code: v["code"].as_i64().unwrap_or(-32603),
            kind: v["data"]["kind"].as_str().unwrap_or("internal").into(),
            message: v["message"].as_str().unwrap_or_default().into(),
            hint: v["data"]["hint"].as_str().unwrap_or_default().into(),
        }
    }

    /// The CLI's exit code for this kind. Stable once announced.
    pub fn exit_code(&self) -> i32 {
        match self.kind.as_str() {
            "usage" => 2,
            "not_found" => 3,
            "machine_unknown" => 4,
            "unavailable" => 5,
            "invalid_params" => 6,
            "needs_target" => 7,
            "config" => 8,
            "blocked" => 9,
            "status_mismatch" => 10,
            "finished" => 11,
            "cancelled" => 130,
            _ => 1,
        }
    }
}

pub fn parse_params<P: DeserializeOwned>(params: Value) -> Result<P, RpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

pub fn response(id: Value, result: Result<Value, RpcError>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(err) => json!({"jsonrpc": "2.0", "id": id, "error": err.to_json()}),
    }
}

fn ref_name(schema: &Value) -> Option<&str> {
    schema.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}

/// Inlines each method's params (readable per method, like MCP tools);
/// results and nested types stay shared definitions.
/// The API as an OpenRPC document (what OpenAPI is to HTTP): methods with named
/// params, result and errors, shared types under `components/schemas`.
pub fn openrpc(methods: Vec<Value>, defs: Map<String, Value>, version: &str, generation: u32) -> Value {
    let methods: Vec<Value> = methods
        .into_iter()
        .map(|m| {
            let required: Vec<&str> =
                m["params"]["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let params: Vec<Value> = m["params"]["properties"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, schema)| {
                    let mut p = json!({"name": name, "required": required.contains(&name.as_str()), "schema": schema});
                    if let Some(d) = schema.get("description") {
                        p["description"] = d.clone();
                    }
                    p
                })
                .collect();
            let errors: Vec<Value> = m["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|kind| {
                    let code = match kind {
                        "invalid_params" => -32602,
                        "internal" => -32603,
                        _ => -32000,
                    };
                    json!({"code": code, "message": kind, "data": {"kind": kind}})
                })
                .collect();
            let result = match m["name"].as_str() {
                Some("rpc.discover") => json!({"$ref": "https://meta.open-rpc.org/"}),
                _ => m["result"].clone(),
            };
            json!({
                "name": m["name"], "description": m["description"], "paramStructure": "by-name",
                "params": params, "result": {"name": "result", "schema": result}, "errors": errors,
            })
        })
        .collect();
    let doc = json!({
        "openrpc": "1.3.2",
        "info": {
            "title": "herdr-core",
            "version": version,
            "description": format!("Terminal runtime for coding agents. JSON-RPC 2.0, one message per line on the hc socket \
                (or framed after `hello`). Protocol generation {generation}. Errors carry a stable `data.kind`."),
            "license": {"name": "Apache-2.0"},
        },
        "methods": methods,
        "components": {"schemas": defs},
    });
    serde_json::from_str(&doc.to_string().replace("\"#/$defs/", "\"#/components/schemas/")).unwrap_or(doc)
}

pub fn share_definitions(mut methods: Vec<Value>, mut defs: Map<String, Value>) -> (Vec<Value>, Map<String, Value>) {
    let used_elsewhere = |name: &str, defs: &Map<String, Value>, methods: &[Value]| {
        let needle = format!("\"#/$defs/{name}\"");
        let in_results = methods.iter().any(|m| m["result"].to_string().contains(&needle));
        in_results || defs.iter().any(|(k, v)| k != name && v.to_string().contains(&needle))
    };
    let params: Vec<Option<String>> = methods.iter().map(|m| ref_name(&m["params"]).map(String::from)).collect();
    for (m, name) in methods.iter_mut().zip(&params) {
        if let Some(name) = name {
            m["params"] = defs.get(name).cloned().unwrap_or_default();
        }
    }
    for name in params.iter().flatten() {
        if !used_elsewhere(name, &defs, &methods) {
            defs.remove(name);
        }
    }
    (methods, defs)
}

/// One table gives dispatch, `hello`'s method list and `rpc.discover`.
macro_rules! methods {
    ($ctx:ty; $( $name:literal => $handler:path, $params:ty => $result:ty,
        $desc:literal, errors [$($err:literal),*]; )*) => {
        pub const METHOD_NAMES: &[&str] = &[$($name),*];

        /// Every method, and the types they share (defined once, under `$defs`).
        pub fn discover() -> (Vec<serde_json::Value>, serde_json::Map<String, serde_json::Value>) {
            let mut generator = schemars::generate::SchemaSettings::draft2020_12().into_generator();
            let methods: Vec<serde_json::Value> = vec![$(serde_json::json!({
                "name": $name,
                "description": $desc,
                "params": generator.subschema_for::<$params>(),
                "result": generator.subschema_for::<$result>(),
                "errors": [$($err),*],
            })),*];
            $crate::rpc::share_definitions(methods, generator.take_definitions(true))
        }

        pub async fn dispatch(ctx: $ctx, method: &str, params: serde_json::Value)
            -> Result<serde_json::Value, $crate::rpc::RpcError> {
            match method {
                $($name => {
                    let params: $params = $crate::rpc::parse_params(params)?;
                    let result: $result = $handler(ctx, params).await?;
                    serde_json::to_value(result)
                        .map_err(|e| $crate::rpc::RpcError::internal(e.to_string()))
                })*
                _ => Err($crate::rpc::RpcError::unknown_method(method)),
            }
        }
    };
}
pub(crate) use methods;
