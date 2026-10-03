mod common;
use futures_util::{SinkExt, StreamExt};
use hark::{
    registry::{ModelWorker, Registry},
    server::{self, Event, Hub},
    websocket::{self, Options},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixStream},
    sync::watch,
    task::JoinHandle,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        protocol::frame::{
            Frame,
            coding::{Data, OpCode},
        },
    },
};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Service {
    url: String,
    path: std::path::PathBuf,
    hub: Hub,
    models: Registry,
    stop: watch::Sender<bool>,
    ws: JoinHandle<anyhow::Result<()>>,
    uds: JoinHandle<anyhow::Result<()>>,
    _worker: ModelWorker,
    _guard: server::SocketGuard,
    _directory: tempfile::TempDir,
}

fn registry() -> (Registry, ModelWorker) {
    Registry::start(
        vec![common::model("hello"), common::model("other")],
        Arc::new(|word| {
            anyhow::ensure!(word != "fail", "test model generation failure");
            Ok(common::model(word))
        }),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
}

impl Service {
    async fn start(registry: (Registry, ModelWorker), options: Options) -> Self {
        let (models, worker) = registry;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hark.sock");
        let (listener, guard) = server::bind(&path).unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", tcp.local_addr().unwrap());
        let hub = Hub::default();
        let (stop, shutdown) = watch::channel(false);
        let uds = tokio::spawn(server::serve(
            listener,
            hub.clone(),
            models.clone(),
            4,
            shutdown.clone(),
        ));
        let ws = tokio::spawn(websocket::serve(
            tcp,
            hub.clone(),
            models.clone(),
            4,
            options,
            shutdown,
        ));
        Self {
            url,
            path,
            hub,
            models,
            stop,
            ws,
            uds,
            _worker: worker,
            _guard: guard,
            _directory: directory,
        }
    }

    async fn connect(&self) -> Client {
        connect_async(&self.url).await.unwrap().0
    }

    async fn finish(self) {
        self.stop.send(true).unwrap();
        self.ws.await.unwrap().unwrap();
        self.uds.await.unwrap().unwrap();
        assert_eq!(self.hub.subscriber_count(), 0);
    }
}

async fn send(client: &mut Client, value: Value) {
    client
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn receive(client: &mut Client) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(7), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(text) = message else {
        panic!("expected JSON text, got {message:?}")
    };
    serde_json::from_str(&text).unwrap()
}

async fn closed(client: &mut Client) {
    tokio::time::timeout(Duration::from_secs(2), async {
        match client.next().await {
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {}
            other => panic!("expected close, got {other:?}"),
        }
    })
    .await
    .unwrap();
}

async fn count(hub: &Hub, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while hub.subscriber_count() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn uds_receive(reader: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn uds_and_websocket_share_readiness_detection_ids_filtering_and_feedback() {
    let service = Service::start(registry(), Options::default()).await;
    let mut ws = service.connect().await;
    send(&mut ws, json!({"subscribe": ["hello"]})).await;
    let mut uds = UnixStream::connect(&service.path).await.unwrap();
    uds.write_all(b"{\"subscribe\":[\"hello\"]}\n")
        .await
        .unwrap();
    let mut uds = BufReader::new(uds);
    count(&service.hub, 2).await;
    let id = service.models.feedback.record("hello", 0.08);
    for word in ["other", "hello"] {
        service.hub.publish(Event {
            event: "detected".into(),
            event_id: id.clone(),
            word: word.into(),
            ts: 1.0,
            score: 0.9,
        });
    }
    // A detection already queued at registration must still follow ready.
    assert_eq!(
        receive(&mut ws).await,
        json!({"event":"ready", "words":["hello"]})
    );
    assert_eq!(uds_receive(&mut uds).await["event"], "ready");
    let detected = receive(&mut ws).await;
    assert_eq!(detected["word"], "hello");
    assert_eq!(detected["event_id"], id);
    assert_eq!(uds_receive(&mut uds).await, detected);
    let report = json!({"feedback": {"event_id":id, "label":"false_positive"}});
    send(&mut ws, report.clone()).await;
    let reply = receive(&mut ws).await;
    assert_eq!(reply["status"], "applied");
    uds.get_mut()
        .write_all(format!("{report}\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(uds_receive(&mut uds).await, reply);
    ws.close(None).await.unwrap();
    closed(&mut ws).await;
    count(&service.hub, 1).await;
    let mut reconnect = service.connect().await;
    send(&mut reconnect, json!({"subscribe":["hello"]})).await;
    assert_eq!(receive(&mut reconnect).await["event"], "ready");
    send(&mut reconnect, report).await;
    assert_eq!(receive(&mut reconnect).await, reply);
    service.finish().await;
    closed(&mut reconnect).await;
    assert_eq!(uds.read(&mut [0]).await.unwrap(), 0);
}

#[tokio::test]
async fn invalid_subscriptions_and_feedback_return_errors_then_close() {
    let service = Service::start(registry(), Options::default()).await;
    for message in [
        Message::Text("{\"subscribe\":[]}".into()),
        Message::Text("{\"subscribe\":[\"hello\"],\"extra\":true}".into()),
        Message::Text("{\"subscribe\":[\"hello\"]}\n{}".into()),
        Message::Binary(b"{}".to_vec().into()),
        Message::Text(" ".repeat(4097).into()),
    ] {
        let mut client = service.connect().await;
        client.send(message).await.unwrap();
        assert_eq!(receive(&mut client).await["error"], "invalid_subscription");
        closed(&mut client).await;
    }
    for message in [
        Message::Text("{\"subscribe\":[\"hello\"]}".into()),
        Message::Text("{\"feedback\":{\"event_id\":\"x\",\"label\":\"unsupported_label\"}}".into()),
        Message::Binary(b"{}".to_vec().into()),
    ] {
        let mut client = service.connect().await;
        send(&mut client, json!({"subscribe":["hello"]})).await;
        assert_eq!(receive(&mut client).await["event"], "ready");
        send(
            &mut client,
            json!({"feedback":{"event_id":"unknown", "label":"false_positive"}}),
        )
        .await;
        assert_eq!(receive(&mut client).await["error"], "feedback_rejected");
        client.send(message).await.unwrap();
        assert_eq!(receive(&mut client).await["error"], "invalid_feedback");
        closed(&mut client).await;
    }
    service.finish().await;
}

#[tokio::test]
async fn fragmented_messages_and_ping_are_supported_without_changing_json_framing() {
    let service = Service::start(registry(), Options::default()).await;
    let mut client = service.connect().await;
    client
        .send(Message::Frame(Frame::message(
            b"{\"subscribe\":".to_vec(),
            OpCode::Data(Data::Text),
            false,
        )))
        .await
        .unwrap();
    client
        .send(Message::Ping(b"alive".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Pong(b"alive".to_vec().into())
    );
    client
        .send(Message::Frame(Frame::message(
            b"[\"hello\"]}".to_vec(),
            OpCode::Data(Data::Continue),
            true,
        )))
        .await
        .unwrap();
    assert_eq!(receive(&mut client).await["event"], "ready");
    client
        .send(Message::Ping(b"ready".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Pong(b"ready".to_vec().into())
    );
    client
        .send(Message::Frame(Frame::message(
            vec![b' '; 2300],
            OpCode::Data(Data::Text),
            false,
        )))
        .await
        .unwrap();
    client
        .send(Message::Frame(Frame::message(
            vec![b' '; 2300],
            OpCode::Data(Data::Continue),
            true,
        )))
        .await
        .unwrap();
    assert_eq!(receive(&mut client).await["error"], "invalid_feedback");
    closed(&mut client).await;
    service.finish().await;
}

#[tokio::test]
async fn preparation_handles_pings_disconnects_and_failure() {
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    let (release, gate) = std::sync::mpsc::sync_channel(1);
    let gate = std::sync::Mutex::new(gate);
    let registry = Registry::start(
        vec![],
        Arc::new(move |word| {
            anyhow::ensure!(word != "fail", "expected failure");
            signal.notify_one();
            gate.lock().unwrap().recv_timeout(Duration::from_secs(5))?;
            Ok(common::model(word))
        }),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let service = Service::start(registry, Options::default()).await;
    let mut client = service.connect().await;
    send(&mut client, json!({"subscribe":["new-word"]})).await;
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    client
        .send(Message::Ping(b"waiting".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Pong(b"waiting".to_vec().into())
    );
    assert_eq!(service.hub.subscriber_count(), 0);
    client.close(None).await.unwrap();
    closed(&mut client).await;
    release.send(()).unwrap();
    let mut reconnect = service.connect().await;
    send(&mut reconnect, json!({"subscribe":["new-word"]})).await;
    assert_eq!(receive(&mut reconnect).await["event"], "ready");
    let mut failed = service.connect().await;
    send(&mut failed, json!({"subscribe":["fail"]})).await;
    assert_eq!(receive(&mut failed).await["error"], "model_unavailable");
    closed(&mut failed).await;
    service.finish().await;
}

#[tokio::test]
async fn path_and_browser_origin_policy_do_not_require_authentication() {
    let service = Service::start(
        registry(),
        Options {
            allowed_origins: vec!["http://localhost:8000".into()],
        },
    )
    .await;
    for (url, origin, expected) in [
        (service.url.replace("/ws", "/wrong"), None, 404),
        (service.url.clone(), Some("https://unlisted.example"), 403),
    ] {
        let mut request = url.into_client_request().unwrap();
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
        }
        let error = connect_async(request).await.err().unwrap();
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(response.status().as_u16(), expected);
    }
    for origin in [None, Some("http://localhost:8000")] {
        let mut request = service.url.clone().into_client_request().unwrap();
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
        }
        let (mut client, _) = connect_async(request).await.unwrap();
        send(&mut client, json!({"subscribe":["hello"]})).await;
        assert_eq!(receive(&mut client).await["event"], "ready");
    }
    service.finish().await;
}

#[tokio::test]
async fn idle_handshake_and_subscription_have_deadlines() {
    let service = Service::start(registry(), Options::default()).await;
    let address = service
        .url
        .trim_start_matches("ws://")
        .trim_end_matches("/ws");
    let mut stalled = TcpStream::connect(address).await.unwrap();
    let mut client = service.connect().await;
    assert_eq!(receive(&mut client).await["error"], "invalid_subscription");
    closed(&mut client).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stalled.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    service.finish().await;
}

async fn python_client(args: Vec<String>) -> Vec<Value> {
    let output = tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new("python3")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "Python client timed out: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines
}

#[tokio::test]
#[ignore = "requires python3 and examples/requirements-websocket.txt"]
async fn python_example_waits_for_ready_and_reports_feedback() {
    let service = Service::start(registry(), Options::default()).await;
    for websocket in [true, false] {
        let negative = service.models.feedback.record("hello", 0.08);
        let positive = service.models.feedback.record("hello", 0.04);
        let (script, endpoint) = if websocket {
            ("examples/subscribe_ws.py", service.url.clone())
        } else {
            (
                "examples/feedback.py",
                service.path.to_str().unwrap().into(),
            )
        };
        for (id, label, expected) in [
            (Some(negative), "false_positive", "applied"),
            (Some(positive), "true_positive", "recorded"),
            (None, "", "reset"),
        ] {
            let mut args: Vec<String> = vec![script.into(), endpoint.clone(), "hello".into()];
            if let Some(id) = &id {
                if websocket {
                    args.push("--feedback".into());
                }
                args.extend([id.clone(), "--label".into(), label.into()]);
            } else {
                args.push("--reset".into());
            }
            let lines = python_client(args).await;
            if websocket {
                assert_eq!(lines[0]["event"], "ready");
            }
            let reply = lines.last().unwrap();
            assert_eq!(reply["status"], expected);
            if let Some(id) = id {
                assert_eq!(reply["event_id"], id);
                assert_eq!(reply["label"], label);
            } else {
                assert_eq!(reply["event"], "reset_result");
            }
        }
    }
    service.finish().await;
}

#[tokio::test]
async fn positive_feedback_and_reset_are_shared_across_transports() {
    let service = Service::start(registry(), Options::default()).await;
    let mut ws = service.connect().await;
    send(&mut ws, json!({"subscribe":["hello"]})).await;
    assert_eq!(receive(&mut ws).await["event"], "ready");
    let mut uds = UnixStream::connect(&service.path).await.unwrap();
    uds.write_all(b"{\"subscribe\":[\"hello\"]}\n")
        .await
        .unwrap();
    let mut uds = BufReader::new(uds);
    assert_eq!(uds_receive(&mut uds).await["event"], "ready");
    let id = service.models.feedback.record("hello", 0.08);
    let report = json!({"feedback":{"event_id": id, "label":"true_positive"}});
    send(&mut ws, report.clone()).await;
    let reply = receive(&mut ws).await;
    assert_eq!(reply["status"], "recorded");
    assert_eq!(reply["label"], "true_positive");
    uds.get_mut()
        .write_all(format!("{report}\n").as_bytes())
        .await
        .unwrap();
    assert_eq!(uds_receive(&mut uds).await, reply);
    send(
        &mut ws,
        json!({"feedback":{"event_id": id,"label":"false_positive"}}),
    )
    .await;
    assert_eq!(receive(&mut ws).await["error"], "feedback_rejected");
    send(&mut ws, json!({"reset":{"word":"other"}})).await;
    assert_eq!(receive(&mut ws).await["error"], "reset_rejected");
    uds.get_mut()
        .write_all(b"{\"reset\":{\"word\":\"hello\"}}\n")
        .await
        .unwrap();
    assert_eq!(uds_receive(&mut uds).await["event"], "reset_result");
    send(&mut ws, report).await;
    assert_eq!(receive(&mut ws).await["error"], "feedback_rejected");
    send(&mut ws, json!({"reset":{"word":"hello"}})).await;
    assert_eq!(receive(&mut ws).await["status"], "reset");
    let id = service.models.feedback.record("hello", 0.07);
    send(
        &mut ws,
        json!({"feedback":{"event_id":id,"label":"false_positive"}}),
    )
    .await;
    assert_eq!(receive(&mut ws).await["status"], "applied");
    assert_eq!(service.hub.subscriber_count(), 2);
    service.finish().await;
}
