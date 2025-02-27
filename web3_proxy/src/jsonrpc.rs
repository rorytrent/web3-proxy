use derive_more::From;
use ethers::prelude::{HttpClientError, ProviderError, WsClientError};
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::{to_raw_value, RawValue};
use std::fmt;

// this is used by serde
#[allow(dead_code)]
fn default_jsonrpc() -> String {
    "2.0".to_string()
}

#[derive(Clone, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    // TODO: skip jsonrpc entirely? its against spec to drop it, but some servers bad
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    /// id could be a stricter type, but many rpcs do things against the spec
    pub id: Box<RawValue>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

impl fmt::Debug for JsonRpcRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        // TODO: how should we include params in this? maybe just the length?
        f.debug_struct("JsonRpcRequest")
            .field("id", &self.id)
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

/// Requests can come in multiple formats
#[derive(Debug, From)]
pub enum JsonRpcRequestEnum {
    Batch(Vec<JsonRpcRequest>),
    Single(JsonRpcRequest),
}

impl<'de> Deserialize<'de> for JsonRpcRequestEnum {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            JsonRpc,
            Id,
            Method,
            Params,
            // TODO: jsonrpc here, too?
        }

        struct JsonRpcBatchVisitor;

        impl<'de> Visitor<'de> for JsonRpcBatchVisitor {
            type Value = JsonRpcRequestEnum;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("JsonRpcRequestEnum")
            }

            fn visit_seq<V>(self, mut seq: V) -> Result<JsonRpcRequestEnum, V::Error>
            where
                V: SeqAccess<'de>,
            {
                // TODO: what size should we use as the default?
                let mut batch: Vec<JsonRpcRequest> =
                    Vec::with_capacity(seq.size_hint().unwrap_or(10));

                while let Ok(Some(s)) = seq.next_element::<JsonRpcRequest>() {
                    batch.push(s);
                }

                Ok(JsonRpcRequestEnum::Batch(batch))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                // TODO: i feel like this should be easier
                let mut jsonrpc = None;
                let mut id = None;
                let mut method = None;
                let mut params = None;

                while let Some(key) = map.next_key()? {
                    match key {
                        Field::JsonRpc => {
                            // throw away the value
                            // TODO: should we check that it's 2.0?
                            // TODO: how do we skip over this value entirely?
                            jsonrpc = Some(map.next_value()?);
                        }
                        Field::Id => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id = Some(map.next_value()?);
                        }
                        Field::Method => {
                            if method.is_some() {
                                return Err(de::Error::duplicate_field("method"));
                            }
                            method = Some(map.next_value()?);
                        }
                        Field::Params => {
                            if params.is_some() {
                                return Err(de::Error::duplicate_field("params"));
                            }
                            params = Some(map.next_value()?);
                        }
                    }
                }

                // some providers don't follow the spec and dont include the jsonrpc key
                // i think "2.0" should be a fine default to handle these incompatible clones
                let jsonrpc = jsonrpc.unwrap_or_else(|| "2.0".to_string());
                // TODO: Errors returned by the try operator get shown in an ugly way
                let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
                let method = method.ok_or_else(|| de::Error::missing_field("method"))?;

                let params: Option<serde_json::Value> = match params {
                    None => Some(serde_json::Value::Array(vec![])),
                    Some(x) => Some(x),
                };

                let single = JsonRpcRequest {
                    jsonrpc,
                    id,
                    method,
                    params,
                };

                Ok(JsonRpcRequestEnum::Single(single))
            }
        }

        let batch_visitor = JsonRpcBatchVisitor {};

        deserializer.deserialize_any(batch_visitor)
    }
}

// TODO: impl Error on this?
/// All jsonrpc errors use this structure
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct JsonRpcErrorData {
    /// The error code
    pub code: i64,
    /// The error message
    pub message: String,
    /// Additional data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A complete response
/// TODO: better Debug response
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JsonRpcForwardedResponse {
    // TODO: jsonrpc a &str?
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    pub id: Box<RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorData>,
}

impl JsonRpcRequest {
    pub fn num_bytes(&self) -> usize {
        // TODO: not sure how to do this without wasting a ton of allocations
        serde_json::to_string(self)
            .expect("this should always be valid json")
            .len()
    }
}

impl JsonRpcForwardedResponse {
    pub fn from_anyhow_error(
        err: anyhow::Error,
        code: Option<i64>,
        id: Option<Box<RawValue>>,
    ) -> Self {
        let message = format!("{:?}", err);

        Self::from_string(message, code, id)
    }

    pub fn from_str(message: &str, code: Option<i64>, id: Option<Box<RawValue>>) -> Self {
        Self::from_string(message.to_string(), code, id)
    }

    pub fn from_string(message: String, code: Option<i64>, id: Option<Box<RawValue>>) -> Self {
        // TODO: this is too verbose. plenty of errors are valid, like users giving an invalid address. no need to log that
        // TODO: can we somehow get the initial request here? if we put that into a tracing span, will things slow down a ton?
        JsonRpcForwardedResponse {
            jsonrpc: "2.0".to_string(),
            id: id.unwrap_or_else(|| {
                to_raw_value(&json!(None::<Option<()>>)).expect("null id should always work")
            }),
            result: None,
            error: Some(JsonRpcErrorData {
                code: code.unwrap_or(-32099),
                message,
                // TODO: accept data as an argument
                data: None,
            }),
        }
    }

    pub fn from_response(partial_response: Box<RawValue>, id: Box<RawValue>) -> Self {
        JsonRpcForwardedResponse {
            jsonrpc: "2.0".to_string(),
            id,
            // TODO: since we only use the result here, should that be all we return from try_send_request?
            result: Some(partial_response),
            error: None,
        }
    }

    pub fn from_value(partial_response: serde_json::Value, id: Box<RawValue>) -> Self {
        let partial_response =
            to_raw_value(&partial_response).expect("Value to RawValue should always work");

        JsonRpcForwardedResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(partial_response),
            error: None,
        }
    }

    pub fn from_ethers_error(e: ProviderError, id: Box<RawValue>) -> anyhow::Result<Self> {
        // TODO: move turning ClientError into json to a helper function?
        let code;
        let message: String;
        let data;

        match e {
            ProviderError::JsonRpcClientError(e) => {
                // TODO: check what type the provider is rather than trying to downcast both types of errors
                let e = e.downcast::<HttpClientError>();

                if let Ok(e) = e {
                    match *e {
                        HttpClientError::JsonRpcError(e) => {
                            code = e.code;
                            message = e.message.clone();
                            data = e.data;
                        }
                        e => {
                            // this is not an rpc error. keep it as an error
                            return Err(e.into());
                        }
                    }
                } else {
                    // it wasn't an HttpClientError. try WsClientError
                    let e = e.unwrap_err().downcast::<WsClientError>();

                    if let Ok(e) = e {
                        match *e {
                            WsClientError::JsonRpcError(e) => {
                                code = e.code;
                                message = e.message.clone();
                                data = e.data;
                            }
                            e => {
                                // this is not an rpc error. keep it as an error
                                return Err(e.into());
                            }
                        }
                    } else {
                        unimplemented!();
                    }
                }
            }
            e => return Err(e.into()),
        }

        Ok(Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcErrorData {
                code,
                message,
                data,
            }),
        })
    }

    pub fn try_from_response_result(
        result: Result<Box<RawValue>, ProviderError>,
        id: Box<RawValue>,
    ) -> anyhow::Result<Self> {
        match result {
            Ok(response) => Ok(Self::from_response(response, id)),
            Err(e) => Self::from_ethers_error(e, id),
        }
    }

    pub fn num_bytes(&self) -> usize {
        // TODO: not sure how to do this without wasting a ton of allocations
        serde_json::to_string(self)
            .expect("this should always be valid json")
            .len()
    }
}

/// JSONRPC Responses can include one or many response objects.
#[derive(Clone, Debug, From, Serialize)]
#[serde(untagged)]
pub enum JsonRpcForwardedResponseEnum {
    Single(JsonRpcForwardedResponse),
    Batch(Vec<JsonRpcForwardedResponse>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_deserialize_single() {
        let input = r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#;

        // test deserializing it directly to a single request object
        let output: JsonRpcRequest = serde_json::from_str(input).unwrap();

        assert_eq!(output.id.to_string(), "1");
        assert_eq!(output.method, "eth_blockNumber");
        assert_eq!(output.params.unwrap().to_string(), "[]");

        // test deserializing it into an enum
        let output: JsonRpcRequestEnum = serde_json::from_str(input).unwrap();

        assert!(matches!(output, JsonRpcRequestEnum::Single(_)));
    }

    #[test]
    fn this_deserialize_batch() {
        let input = r#"[{"jsonrpc":"2.0","method":"eth_getCode","params":["0x5ba1e12693dc8f9c48aad8770482f4739beed696","0xe0e6a4"],"id":27},{"jsonrpc":"2.0","method":"eth_getTransactionCount","params":["0x5ba1e12693dc8f9c48aad8770482f4739beed696","0xe0e6a4"],"id":28},{"jsonrpc":"2.0","method":"eth_getBalance","params":["0x5ba1e12693dc8f9c48aad8770482f4739beed696","0xe0e6a4"],"id":29}]"#;

        // test deserializing it directly to a batch of request objects
        let output: Vec<JsonRpcRequest> = serde_json::from_str(input).unwrap();

        assert_eq!(output.len(), 3);

        assert_eq!(output[0].id.to_string(), "27");
        assert_eq!(output[0].method, "eth_getCode");
        assert_eq!(
            output[0].params.as_ref().unwrap().to_string(),
            r#"["0x5ba1e12693dc8f9c48aad8770482f4739beed696","0xe0e6a4"]"#
        );

        assert_eq!(output[1].id.to_string(), "28");
        assert_eq!(output[2].id.to_string(), "29");

        // test deserializing it into an enum
        let output: JsonRpcRequestEnum = serde_json::from_str(input).unwrap();

        assert!(matches!(output, JsonRpcRequestEnum::Batch(_)));
    }
}
