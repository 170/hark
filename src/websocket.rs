//! Optional WebSocket listener sharing the UDS application protocol and hub.
use crate::{
    registry::Registry,
    server::{self, Hub},
    transport::{self, MAX_MESSAGE},
};
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
};
use tokio_tungstenite::{
    accept_hdr_async_with_config,
    tungstenite::{
        handshake::server::{Request, Response},
        http::StatusCode,
        protocol::WebSocketConfig,
    },
};

#[derive(Clone, Default)]
pub struct Options {
    /// Exact browser Origin values allowed in the handshake. Non-browser clients
    /// normally send no Origin and do not need an entry. This is not authentication.
    pub allowed_origins: Vec<String>,
}

async fn client(
    stream: TcpStream,
    hub: Hub,
    models: Registry,
    capacity: usize,
    options: Arc<Options>,
) -> Result<()> {
    let peer = format!("websocket peer={}", stream.peer_addr()?);
    let config = WebSocketConfig::default()
        .read_buffer_size(MAX_MESSAGE)
        .write_buffer_size(0)
        .max_write_buffer_size(64 * 1024)
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    // Tungstenite's callback requires an unboxed HTTP response as the error type.
    #[allow(clippy::result_large_err)]
    let callback = move |request: &Request, response: Response| {
        let rejected = if request.uri().path() != "/ws" || request.uri().query().is_some() {
            Some((StatusCode::NOT_FOUND, "WebSocket endpoint is /ws"))
        } else if request.headers().get_all("origin").iter().count() > 1
            || request.headers().get("origin").is_some_and(|origin| {
                !options
                    .allowed_origins
                    .iter()
                    .any(|allowed| origin.as_bytes() == allowed.as_bytes())
            })
        {
            Some((StatusCode::FORBIDDEN, "browser Origin is not allowed"))
        } else {
            None
        };
        if let Some((status, message)) = rejected {
            let mut error =
                tokio_tungstenite::tungstenite::http::Response::new(Some(message.into()));
            *error.status_mut() = status;
            Err(error)
        } else {
            Ok(response)
        }
    };
    let stream = tokio::time::timeout(
        Duration::from_secs(5),
        accept_hdr_async_with_config(stream, callback, Some(config)),
    )
    .await??;
    let (read, write) = transport::websocket(stream);
    server::run_session(read, write, hub, models, capacity, peer).await
}

pub async fn serve(
    listener: TcpListener,
    hub: Hub,
    models: Registry,
    capacity: usize,
    options: Options,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    anyhow::ensure!(capacity > 0, "queue capacity must be positive");
    let options = Arc::new(options);
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            result = clients.join_next(), if !clients.is_empty() => {
                if let Some(Err(error)) = result { eprintln!("WebSocket task failed: {error}"); }
            },
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Some(permit) = hub.reserve_client() else { drop(stream); continue; };
                let (hub, models, options) = (hub.clone(), models.clone(), options.clone());
                clients.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = client(stream, hub, models, capacity, options).await {
                        eprintln!("WebSocket client error: {error:#}");
                    }
                });
            }
        }
    }
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    Ok(())
}
