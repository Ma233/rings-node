use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use serde::Deserialize;
use serde::Serialize;

use crate::backend_client::BackendMessage;
use crate::backend_client::HttpServerMessage;
use crate::backend_client::HttpServerRequest;
use crate::backend_client::HttpServerResponse;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::message::Message;
use crate::prelude::*;

#[derive(Deserialize, Serialize, Debug)]
pub struct BackendConfig {
    pub http_server: Option<HttpServerConfig>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct HttpServerConfig {
    pub port: u16,
}

pub struct Backend {
    http_server: Option<HttpServer>,
}

pub struct HttpServer {
    client: reqwest::Client,
    port: u16,
}

impl Backend {
    pub fn new(config: BackendConfig) -> Self {
        Self {
            http_server: config.http_server.map(HttpServer::new),
        }
    }
}

impl HttpServer {
    pub fn new(config: HttpServerConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            port: config.port,
        }
    }

    pub async fn execute(&self, request: HttpServerRequest) -> Result<HttpServerResponse> {
        let url = format!(
            "http://localhost:{}/{}",
            self.port,
            request.path.trim_start_matches('/')
        );
        let method = try_into_method(&request.method)?;

        let mut headers = HeaderMap::new();
        for (name, value) in request.headers {
            headers.insert(
                name.parse::<HeaderName>().map_err(|_| {
                    Error::HttpRequestError(format!("Invalid header name: {}", &name))
                })?,
                value.parse().map_err(|_| {
                    Error::HttpRequestError(format!("Invalid header value: {}", &value))
                })?,
            );
        }

        let req = self
            .client
            .request(method, &url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(15));
        let req = request
            .body
            .map_or(req.try_clone().unwrap(), |body| req.body(body));
        let resp = req
            .send()
            .await
            .map_err(|e| Error::HttpRequestError(e.to_string()))?;

        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_str().unwrap().to_string()))
            .collect();
        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::HttpRequestError(e.to_string()))?;

        Ok(HttpServerResponse {
            status,
            headers,
            body: Some(body),
        })
    }
}

fn try_into_method(s: &str) -> Result<http::Method> {
    match s.to_uppercase().as_str() {
        "GET" => Ok(http::Method::GET),
        "POST" => Ok(http::Method::POST),
        "PUT" => Ok(http::Method::PUT),
        "DELETE" => Ok(http::Method::DELETE),
        "HEAD" => Ok(http::Method::HEAD),
        "OPTIONS" => Ok(http::Method::OPTIONS),
        "TRACE" => Ok(http::Method::TRACE),
        "CONNECT" => Ok(http::Method::CONNECT),
        "PATCH" => Ok(http::Method::PATCH),
        _ => Err(Error::HttpRequestError("Invalid HTTP method".to_string())),
    }
}

#[async_trait]
impl MessageCallback for Backend {
    async fn custom_message(
        &self,
        handler: &MessageHandler,
        ctx: &MessagePayload<Message>,
        msg: &MaybeEncrypted<CustomMessage>,
    ) {
        let mut relay = ctx.relay.clone();
        relay.relay(relay.destination, None).unwrap();

        if let Ok(CustomMessage(raw_msg)) = handler.decrypt_msg(msg) {
            if let Ok(msg) = serde_json::from_slice(&raw_msg) {
                match msg {
                    BackendMessage::HttpServer(msg) => match msg {
                        HttpServerMessage::Request(req) => {
                            tracing::info!("Received HTTP server request: {:?}", req);

                            if let Some(ref server) = self.http_server {
                                let resp = server.execute(req).await.unwrap_or_else(|e| {
                                    HttpServerResponse {
                                        status: 500,
                                        headers: vec![],
                                        body: Some(Bytes::from(e.to_string())),
                                    }
                                });
                                tracing::info!("Sending HTTP server response: {:?}", resp);

                                let resp =
                                    BackendMessage::HttpServer(HttpServerMessage::Response(resp));
                                let resp_bytes = serde_json::to_vec(&resp).unwrap();
                                let pubkey = ctx.origin_session_pubkey().unwrap();

                                handler
                                    .send_report_message(
                                        Message::custom(&resp_bytes, Some(pubkey)).unwrap(),
                                        ctx.tx_id,
                                        relay,
                                    )
                                    .await
                                    .unwrap();
                            } else {
                                tracing::warn!("HTTP server is not configured");
                            }
                        }
                        HttpServerMessage::Response(resp) => {
                            println!("HttpServerMessage::Response: {:?}", resp);
                        }
                    },
                }
            }
        }
    }

    async fn builtin_message(&self, _handler: &MessageHandler, _ctx: &MessagePayload<Message>) {}
}