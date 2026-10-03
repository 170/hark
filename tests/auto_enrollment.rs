//! Opt-in integration check using installed TTS voices, without a microphone.
use hark::{
    audio,
    engine::{self, Segmenter},
    enrollment::{ModelCache, SynthesisSettings},
    registry::Registry,
    server::{self, Event, Hub},
};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::watch,
};

#[tokio::test]
#[ignore = "requires installed native TTS voices and permission to create a local socket"]
async fn native_tts_subscription_builds_a_model_and_delivers_an_event() {
    native_subscription("hey-computer").await;
}

#[tokio::test]
#[ignore = "requires installed Japanese TTS voices and permission to create a local socket"]
async fn japanese_subscription_selects_voices_and_becomes_ready() {
    native_subscription("こんにちは").await;
}

async fn native_subscription(word: &str) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("hark.sock");
    let cache = ModelCache::new(
        directory.path().join("models"),
        SynthesisSettings::new(vec![], vec![]).unwrap(),
    );
    let stopping = Arc::new(AtomicBool::new(false));
    let builder_stop = stopping.clone();
    let (models, _worker) = Registry::start(
        vec![],
        Arc::new(move |word| cache.get_or_build(word, &builder_stop)),
        stopping,
    )
    .unwrap();
    let (listener, _guard) = server::bind(&socket).unwrap();
    let hub = Hub::default();
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(server::serve(
        listener,
        hub.clone(),
        models.clone(),
        4,
        shutdown,
    ));
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    stream
        .write_all(format!("{}\n", serde_json::json!({"subscribe": [word]})).as_bytes())
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(180), reader.read_line(&mut ready))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ready).unwrap(),
        serde_json::json!({"event": "ready", "words": [word]})
    );
    let snapshot = models.snapshot();
    assert_eq!(snapshot.len(), 1);
    let model = &snapshot[0];
    assert_eq!(model.word, word);
    assert!([6, 9].contains(&model.templates.len()));
    // Replay actual WAV samples through the same segmenter and feature path
    // used by live capture. Use a different rate and an untrained voice.
    #[cfg(target_os = "macos")]
    let voices = if word == "こんにちは" {
        vec!["Kyoko", "Reed (Japanese (Japan))"]
    } else {
        vec!["Moira"]
    };
    #[cfg(target_os = "linux")]
    let voices = if word == "こんにちは" {
        vec!["ja+m2"]
    } else {
        vec!["en-us+f3"]
    };
    let mut score = 0.0;
    for (i, voice) in voices.iter().enumerate() {
        let wav = directory.path().join(format!("evaluation-{i}.wav"));
        evaluation_wav(&word.replace('-', " "), voice, &wav);
        let samples = audio::read_wav(&wav).unwrap();
        for gain in [1.0, 0.25] {
            let mut segmenter = Segmenter::default();
            let mut stream = vec![0.0; engine::RATE * 2];
            stream.extend(samples.iter().map(|v| v * gain));
            stream.extend(vec![0.0; engine::RATE]);
            let matches: Vec<_> = stream
                .as_chunks::<{ engine::HOP }>()
                .0
                .iter()
                .filter_map(|frame| segmenter.push(frame))
                .map(|utterance| model.evaluate(&engine::features(&utterance)))
                .collect();
            eprintln!("replay word={word:?} voice={voice:?} gain={gain}: {matches:?}");
            assert_eq!(matches.len(), 1, "expected one complete utterance");
            assert!(
                matches[0].detected,
                "untrained audio was rejected: {matches:?}"
            );
            score = matches[0].score;
        }
    }
    hub.publish(Event {
        event: "detected".into(),
        event_id: "test-event".into(),
        word: model.word.clone(),
        ts: 1.0,
        score,
    });
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::from_str::<Event>(&line).unwrap().word, word);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

fn evaluation_wav(text: &str, voice: &str, path: &Path) {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("say");
        command
            .args([
                "-v",
                voice,
                "-r",
                "190",
                "--file-format=WAVE",
                "--data-format=LEI16@16000",
                "-o",
            ])
            .arg(path);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("espeak-ng");
        command
            .args(["-v", voice, "-s", "190", "--stdin", "-w"])
            .arg(path);
        command
    };
    let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}
