mod common;
use hark::{
    registry::{ModelWorker, Registry},
    server::{self, Event, Hub},
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::watch,
};

fn preloaded(words: &[&str]) -> (Registry, ModelWorker) {
    Registry::start(
        words.iter().map(|word| common::model(word)).collect(),
        Arc::new(|_| anyhow::bail!("unexpected model build")),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
}

async fn expect_ready(reader: &mut BufReader<UnixStream>, words: &[&str]) {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap(),
        serde_json::json!({"event": "ready", "words": words})
    );
}

async fn until_count(hub: &Hub, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while hub.subscriber_count() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn read_json(reader: &mut BufReader<UnixStream>) -> serde_json::Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn feedback_on_the_subscription_connection_tunes_detection_and_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    let (listener, _guard) = server::bind(&path).unwrap();
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let (models, _worker) = preloaded(&["hello", "other"]);
    let task = tokio::spawn(server::serve(
        listener,
        hub.clone(),
        models.clone(),
        4,
        shutdown,
    ));
    let mut stream = UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(b"{\"subscribe\":[\"hello\"]}\n")
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);
    expect_ready(&mut reader, &["hello"]).await;
    let model = models
        .snapshot()
        .into_iter()
        .find(|model| model.word == "hello")
        .unwrap();
    let features = vec![[0.2; 13]; 15];
    let before = models.feedback.adjust("hello", model.evaluate(&features));
    assert!(before.detected);
    let event_id = models.feedback.record("hello", before.distance.unwrap());
    hub.publish(Event {
        event: "detected".into(),
        event_id: event_id.clone(),
        word: "hello".into(),
        ts: 1.0,
        score: before.score,
    });
    assert_eq!(read_json(&mut reader).await["event_id"], event_id);
    for bad_id in ["unknown".into(), models.feedback.record("other", 0.08)] {
        let report =
            serde_json::json!({"feedback": {"event_id": bad_id, "label": "false_positive"}});
        reader
            .get_mut()
            .write_all(format!("{report}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(read_json(&mut reader).await["error"], "feedback_rejected");
    }
    let report = serde_json::json!({"feedback": {"event_id": event_id, "label": "false_positive"}});
    let line = format!("{report}\n");
    reader
        .get_mut()
        .write_all(&line.as_bytes()[..12])
        .await
        .unwrap();
    reader
        .get_mut()
        .write_all(&line.as_bytes()[12..])
        .await
        .unwrap();
    let applied = read_json(&mut reader).await;
    assert_eq!(applied["event"], "feedback_result");
    assert_eq!(applied["status"], "applied");
    assert!(
        !models
            .feedback
            .adjust("hello", model.evaluate(&features))
            .detected
    );
    assert_eq!(hub.subscriber_count(), 1);
    // A fresh connection may retry a known report, without applying it twice.
    drop(reader);
    until_count(&hub, 0).await;
    let mut stream = UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(b"{\"subscribe\":[\"hello\"]}\n")
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);
    expect_ready(&mut reader, &["hello"]).await;
    reader.get_mut().write_all(line.as_bytes()).await.unwrap();
    assert_eq!(read_json(&mut reader).await, applied);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn malformed_feedback_returns_error_before_disconnect() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    let (listener, _guard) = server::bind(&path).unwrap();
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let (models, _worker) = preloaded(&["hello"]);
    let task = tokio::spawn(server::serve(listener, hub.clone(), models, 4, shutdown));
    for request in [
        b"{\"subscribe\":[\"hello\"]}\n".to_vec(),
        b"{\"feedback\":{\"event_id\":\"x\",\"label\":\"unsupported_label\"}}\n".to_vec(),
        b"{\"feedback\":{\"event_id\":\"x\",\"label\":\"false_positive\",\"score\":0}}\n".to_vec(),
        vec![b'x'; 4097],
        vec![0xff, b'\n'],
    ] {
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(b"{\"subscribe\":[\"hello\"]}\n")
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        expect_ready(&mut reader, &["hello"]).await;
        reader.get_mut().write_all(&request).await.unwrap();
        assert_eq!(read_json(&mut reader).await["error"], "invalid_feedback");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), reader.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        until_count(&hub, 0).await;
    }
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn subscription_delivery_filtering_disconnect_and_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    let (listener, guard) = server::bind(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o660
    );
    assert!(server::bind(&path).is_err());
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let (models, _worker) = preloaded(&["hey-computer", "other"]);
    let task = tokio::spawn(server::serve(listener, hub.clone(), models, 4, shutdown));
    let mut client = UnixStream::connect(&path).await.unwrap();
    assert_eq!(server::peer_uid(&client).unwrap(), unsafe {
        libc::geteuid()
    });
    // Split a JSON message across writes to exercise framing.
    client.write_all(b"{\"subscribe\":").await.unwrap();
    client.write_all(b"[\"hey-computer\"]}\n").await.unwrap();
    until_count(&hub, 1).await;
    hub.publish(Event {
        event: "detected".into(),
        event_id: "test-event".into(),
        word: "other".into(),
        ts: 0.0,
        score: 0.5,
    });
    let event = Event {
        event: "detected".into(),
        event_id: "test-event".into(),
        word: "hey-computer".into(),
        ts: 1759492560.123,
        score: 0.93,
    };
    hub.publish(event);
    let mut reader = BufReader::new(client);
    expect_ready(&mut reader, &["hey-computer"]).await;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let received: Event = serde_json::from_str(&line).unwrap();
    assert_eq!(received.word, "hey-computer");
    assert_eq!(received.ts, 1759492560.123);
    assert_eq!(received.score, 0.93);
    drop(reader);
    until_count(&hub, 0).await;
    let mut remaining = UnixStream::connect(&path).await.unwrap();
    remaining
        .write_all(b"{\"subscribe\":[\"other\"]}\n")
        .await
        .unwrap();
    until_count(&hub, 1).await;
    let mut remaining = BufReader::new(remaining);
    expect_ready(&mut remaining, &["other"]).await;
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    until_count(&hub, 0).await;
    let mut byte = [0];
    assert_eq!(remaining.read(&mut byte).await.unwrap(), 0);
    drop(guard);
    assert!(!path.exists());
}

#[tokio::test]
async fn invalid_requests_return_error_then_eof() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    let (listener, _guard) = server::bind(&path).unwrap();
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let (models, _worker) = preloaded(&["hey-computer"]);
    let task = tokio::spawn(server::serve(listener, hub.clone(), models, 1, shutdown));
    for request in [
        b"{\"subscribe\":[]}\n".to_vec(),
        b"{\"subscribe\":[\"../missing\"]}\n".to_vec(),
        vec![b'x'; 4097],
        vec![0xff, b'\n'],
    ] {
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream.write_all(&request).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"],
            "invalid_subscription"
        );
    }
    assert_eq!(hub.subscriber_count(), 0);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn stale_socket_replaced_but_regular_file_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let (listener, guard) = server::bind(&path).unwrap();
    drop(listener);
    drop(guard);
    std::fs::write(&path, b"keep me").unwrap();
    assert!(server::bind(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
}

#[tokio::test]
async fn unknown_word_is_built_and_can_receive_detections_without_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    let (listener, _guard) = server::bind(&path).unwrap();
    let (models, _worker) = Registry::start(
        vec![],
        Arc::new(|word| Ok(common::model(word))),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(server::serve(
        listener,
        hub.clone(),
        models.clone(),
        4,
        shutdown,
    ));
    let mut stream = UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(b"{\"subscribe\":[\"new-word\"]}\n")
        .await
        .unwrap();
    until_count(&hub, 1).await;
    let snapshot = models.snapshot();
    assert_eq!(snapshot[0].word, "new-word");
    let (detected, score) = snapshot[0].compare(&snapshot[0].templates[0]);
    assert!(detected);
    hub.publish(Event {
        event: "detected".into(),
        event_id: "test-event".into(),
        word: snapshot[0].word.clone(),
        ts: 1.0,
        score,
    });
    let mut reader = BufReader::new(stream);
    expect_ready(&mut reader, &["new-word"]).await;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Event>(&line).unwrap().word,
        "new-word"
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn generation_failure_returns_error_and_disconnects_without_subscription() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    let (listener, _guard) = server::bind(&path).unwrap();
    let (models, _worker) = preloaded(&[]);
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(server::serve(listener, hub.clone(), models, 4, shutdown));
    let mut stream = UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(b"{\"subscribe\":[\"new-word\"]}\n")
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"],
        "model_unavailable"
    );
    assert_eq!(hub.subscriber_count(), 0);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn pending_generation_does_not_block_existing_client_and_disconnect_cancels_subscription() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    let (listener, _guard) = server::bind(&path).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    let (release, gate) = std::sync::mpsc::sync_channel(1);
    let gate = std::sync::Mutex::new(gate);
    let (models, _worker) = Registry::start(
        vec![common::model("existing")],
        Arc::new(move |word| {
            signal.notify_one();
            gate.lock().unwrap().recv_timeout(Duration::from_secs(3))?;
            Ok(common::model(word))
        }),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(server::serve(
        listener,
        hub.clone(),
        models.clone(),
        4,
        shutdown,
    ));
    let mut existing = UnixStream::connect(&path).await.unwrap();
    existing
        .write_all(b"{\"subscribe\":[\"existing\"]}\n")
        .await
        .unwrap();
    until_count(&hub, 1).await;
    let mut pending = UnixStream::connect(&path).await.unwrap();
    pending
        .write_all(b"{\"subscribe\":[\"new-word\"]}\n")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    hub.publish(Event {
        event: "detected".into(),
        event_id: "test-event".into(),
        word: "existing".into(),
        ts: 1.0,
        score: 0.9,
    });
    let mut existing = BufReader::new(existing);
    expect_ready(&mut existing, &["existing"]).await;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), existing.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Event>(&line).unwrap().word,
        "existing"
    );
    // Half-close lets us observe the server's EOF while synthesis is still blocked.
    pending.shutdown().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), pending.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    release.send(()).unwrap();
    models
        .ensure(&std::collections::HashSet::from(["new-word".into()]))
        .await
        .unwrap();
    assert_eq!(hub.subscriber_count(), 1);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn ready_waits_for_all_words_and_is_sent_again_on_reconnect() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("hark.sock");
    let (listener, _guard) = server::bind(&path).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    let (release, gate) = std::sync::mpsc::sync_channel(1);
    let gate = std::sync::Mutex::new(gate);
    let (models, _worker) = Registry::start(
        vec![common::model("existing")],
        Arc::new(move |word| {
            signal.notify_one();
            gate.lock().unwrap().recv_timeout(Duration::from_secs(3))?;
            Ok(common::model(word))
        }),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(server::serve(listener, hub.clone(), models, 4, shutdown));
    let request = b"{\"subscribe\":[\"new-word\",\"existing\",\"new-word\"]}\n";
    let mut stream = UnixStream::connect(&path).await.unwrap();
    stream.write_all(request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    // One model is available, but the subscription must wait for the other.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), stream.read(&mut [0]))
            .await
            .is_err()
    );
    assert_eq!(hub.subscriber_count(), 0);
    release.send(()).unwrap();
    let mut reader = BufReader::new(stream);
    expect_ready(&mut reader, &["existing", "new-word"]).await;
    assert_eq!(hub.subscriber_count(), 1);
    drop(reader);
    until_count(&hub, 0).await;
    let mut reconnect = UnixStream::connect(&path).await.unwrap();
    reconnect.write_all(request).await.unwrap();
    let mut reader = BufReader::new(reconnect);
    expect_ready(&mut reader, &["existing", "new-word"]).await;
    // Exactly one ready line is sent per connection, even for duplicate words.
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    let mut remaining = String::new();
    reader.read_to_string(&mut remaining).await.unwrap();
    assert!(remaining.is_empty());
}
