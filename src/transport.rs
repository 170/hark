//! Framing adapters for the shared subscription and feedback session.
use anyhow::{Result, bail, ensure};
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpStream, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

pub(crate) const MAX_MESSAGE: usize = 4096;
type WebSocket = WebSocketStream<TcpStream>;

pub(crate) enum Reader {
    Unix(BufReader<OwnedReadHalf>),
    WebSocket(SplitStream<WebSocket>),
}

pub(crate) enum Writer {
    Unix(OwnedWriteHalf),
    WebSocket(SplitSink<WebSocket, Message>),
}

pub(crate) fn unix(stream: UnixStream) -> (Reader, Writer) {
    let (read, write) = stream.into_split();
    (Reader::Unix(BufReader::new(read)), Writer::Unix(write))
}

pub(crate) fn websocket(stream: WebSocket) -> (Reader, Writer) {
    let (write, read) = stream.split();
    (Reader::WebSocket(read), Writer::WebSocket(write))
}

impl Reader {
    pub(crate) async fn read(&mut self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Unix(reader) => {
                let mut line = Vec::new();
                let count = reader
                    .take((MAX_MESSAGE + 1) as u64)
                    .read_until(b'\n', &mut line)
                    .await?;
                if count == 0 {
                    return Ok(None);
                }
                ensure!(
                    line.len() <= MAX_MESSAGE && line.last() == Some(&b'\n'),
                    "expected a JSON line of at most 4096 bytes"
                );
                Ok(Some(line))
            }
            Self::WebSocket(reader) => {
                while let Some(message) = reader.next().await {
                    match message? {
                        Message::Text(text) => {
                            ensure!(text.len() <= MAX_MESSAGE, "message exceeds 4096 bytes");
                            return Ok(Some(text.as_bytes().to_vec()));
                        }
                        Message::Close(_) => return Ok(None),
                        // Tungstenite queues automatic pong replies and flushes on reads.
                        Message::Ping(_) | Message::Pong(_) => continue,
                        _ => bail!("expected a WebSocket text message containing one JSON object"),
                    }
                }
                Ok(None)
            }
        }
    }
}

impl Writer {
    pub(crate) async fn send(&mut self, value: &Value) -> Result<()> {
        let mut text = serde_json::to_string(value)?;
        match self {
            Self::Unix(writer) => {
                text.push('\n');
                writer.write_all(text.as_bytes()).await?;
            }
            Self::WebSocket(writer) => writer.send(Message::Text(text.into())).await?,
        }
        Ok(())
    }

    pub(crate) async fn close(&mut self) -> Result<()> {
        match self {
            Self::Unix(writer) => writer.shutdown().await?,
            Self::WebSocket(writer) => writer.close().await?,
        }
        Ok(())
    }
}
