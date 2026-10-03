mod common;
use hark::registry::{Registry, spoken_text, validate_word};
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

fn words(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[test]
fn pronunciation_and_identifier_validation() {
    assert_eq!(spoken_text("hey-computer").unwrap(), "hey computer");
    assert_eq!(spoken_text("hey__computer").unwrap(), "hey computer");
    assert_eq!(spoken_text("こんにちは").unwrap(), "こんにちは");
    for word in [
        "",
        " ",
        "hello\nworld",
        "../bad",
        "[[slnc 1000]]",
        "--",
        " padded",
    ] {
        assert!(validate_word(word).is_err(), "{word:?}");
    }
    assert!(validate_word(&"x".repeat(129)).is_err());
}

#[tokio::test]
async fn duplicate_requests_share_work_and_existing_models_remain_available() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let (release, gate) = std::sync::mpsc::sync_channel(1);
    let gate = Mutex::new(gate);
    let counter = calls.clone();
    let signal = started.clone();
    let (registry, _worker) = Registry::start(
        vec![common::model("existing")],
        Arc::new(move |word| {
            counter.fetch_add(1, Ordering::Relaxed);
            signal.notify_one();
            gate.lock().unwrap().recv_timeout(Duration::from_secs(3))?;
            Ok(common::model(word))
        }),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let a = registry.clone();
    let first = tokio::spawn(async move { a.ensure(&words(&["new-word"])).await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let b = registry.clone();
    let second = tokio::spawn(async move { b.ensure(&words(&["new-word"])).await });
    tokio::task::yield_now().await;
    registry.ensure(&words(&["existing"])).await.unwrap();
    assert_eq!(registry.snapshot().len(), 1);
    release.send(()).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    registry.ensure(&words(&["new-word"])).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(registry.snapshot().len(), 2);
}

#[tokio::test]
async fn failures_are_retryable_and_never_install_a_model() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let (registry, _worker) = Registry::start(
        vec![],
        Arc::new(move |word| {
            if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                anyhow::bail!("TTS unavailable");
            }
            Ok(common::model(word))
        }),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert!(registry.ensure(&words(&["new-word"])).await.is_err());
    assert!(registry.snapshot().is_empty());
    registry.ensure(&words(&["new-word"])).await.unwrap();
    assert_eq!(attempts.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn invalid_or_over_limit_requests_start_no_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let (registry, worker) = Registry::start(
        vec![],
        Arc::new(move |word| {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(common::model(word))
        }),
        stop,
    )
    .unwrap();
    assert!(
        registry
            .ensure(&words(&["valid", "../invalid"]))
            .await
            .is_err()
    );
    assert!(
        registry
            .ensure(&(0..17).map(|n| format!("word-{n}")).collect())
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    drop(worker);
    assert!(registry.ensure(&words(&["valid"])).await.is_err());
}

#[tokio::test]
async fn shutdown_cancels_an_in_progress_builder() {
    let stop = Arc::new(AtomicBool::new(false));
    let builder_stop = stop.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let signal = started.clone();
    let (registry, worker) = Registry::start(
        vec![],
        Arc::new(move |_| {
            signal.notify_one();
            while !builder_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            anyhow::bail!("cancelled")
        }),
        stop,
    )
    .unwrap();
    let pending = tokio::spawn(async move { registry.ensure(&words(&["pending"])).await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    drop(worker);
    assert!(pending.await.unwrap().is_err());
}
