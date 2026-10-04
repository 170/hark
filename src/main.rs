use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use hark::{
    audio::{self, Resampler},
    engine::{self, Model, Segmenter},
    enrollment::{ModelCache, SynthesisSettings, load_model, save_model, synthesize_model},
    registry::Registry,
    server::{self, Event, Hub},
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{oneshot, watch};

#[derive(Parser)]
#[command(version, about = "Local wake-word service with synthetic enrollment")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Generate local TTS examples and train without recording your own voice.
    Synthesize {
        #[arg(long)]
        word: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        output: PathBuf,
        /// Comma-separated TTS voices; omitted voices are selected for English or Japanese.
        #[arg(long, value_delimiter = ',')]
        voices: Vec<String>,
        /// Non-wake phrases for automatic threshold calibration (repeatable).
        #[arg(long)]
        negative: Vec<String>,
    },
    /// Train from isolated positive and optional negative WAV files.
    Train {
        #[arg(long)]
        word: String,
        #[arg(long, required = true, num_args = 3..)]
        positive: Vec<PathBuf>,
        #[arg(long, num_args = 1..)]
        negative: Vec<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Compare one isolated WAV to a model without opening the microphone.
    Check {
        #[arg(long)]
        model: PathBuf,
        #[arg(long)]
        wav: PathBuf,
        /// Run the WAV through live speech segmentation with leading/trailing silence.
        #[arg(long)]
        streaming: bool,
    },
    /// Open the microphone and push detections to UDS and optional WebSocket subscribers.
    Serve(ServeOptions),
}

#[derive(clap::Args)]
struct ServeOptions {
    /// Optional models to preload instead of generating them on subscription.
    #[arg(long, num_args = 1..)]
    model: Vec<PathBuf>,
    /// Persistent cache for models generated from subscriptions.
    #[arg(long)]
    model_cache: Option<PathBuf>,
    /// Comma-separated TTS voices; defaults are selected per phrase for English or Japanese.
    #[arg(long, value_delimiter = ',')]
    voices: Vec<String>,
    /// Non-wake phrases used to calibrate every generated model (repeatable).
    #[arg(long)]
    negative: Vec<String>,
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Also listen for unauthenticated WebSocket clients at this IP:PORT (/ws).
    #[arg(long)]
    websocket: Option<SocketAddr>,
    /// Allow this exact browser Origin (repeatable). Clients without Origin are allowed.
    #[arg(long, requires = "websocket")]
    websocket_origin: Vec<String>,
    #[arg(long)]
    device: Option<String>,
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u16).range(1..=4096))]
    queue_capacity: u16,
    /// Log microphone levels, segmentation status, and matching decisions.
    #[arg(long)]
    diagnostics: bool,
}

fn default_socket() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR")
                .context("XDG_RUNTIME_DIR is unset; pass --socket")?,
        )
        .join("hark/hark.sock"))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(
            PathBuf::from(std::env::var_os("HOME").context("HOME is unset; pass --socket")?)
                .join("Library/Caches/hark/hark.sock"),
        )
    }
}

fn detect(
    models: Registry,
    device: Option<String>,
    hub: Hub,
    stop: Arc<AtomicBool>,
    diagnostics: bool,
) -> Result<()> {
    let microphone = audio::open_microphone(device.as_deref())?;
    let mut resampler = Resampler::new(microphone.rate);
    let mut segmenter = Segmenter::default();
    let mut pending = Vec::new();
    let mut last_detection = HashMap::<String, Instant>::new();
    let mut last_audio = Instant::now();
    let mut last_diagnostic = Instant::now();
    let mut peak_rms = 0.0f32;
    eprintln!("calibrating background noise for one second; remain quiet");
    while !stop.load(Ordering::Relaxed) {
        ensure!(
            !microphone.failed.load(Ordering::Relaxed),
            "microphone failed"
        );
        ensure!(
            !microphone.overflow.swap(false, Ordering::Relaxed),
            "audio queue overflow; detector cannot keep up"
        );
        match microphone.samples.recv_timeout(Duration::from_millis(200)) {
            Ok(samples) => {
                last_audio = Instant::now();
                ensure!(
                    samples.iter().all(|v| v.is_finite()),
                    "invalid microphone samples"
                );
                pending.extend(resampler.push(&samples));
                let consumed = pending.len() / engine::HOP * engine::HOP;
                for frame in pending[..consumed].as_chunks::<{ engine::HOP }>().0 {
                    if diagnostics {
                        peak_rms = peak_rms.max(engine::rms(frame));
                    }
                    if let Some(utterance) = segmenter.push(frame) {
                        let features = engine::features(&utterance);
                        for model in models.snapshot() {
                            if last_detection
                                .get(&model.word)
                                .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
                            {
                                continue;
                            }
                            let result = models.feedback.adjust_with_features(
                                &model.word,
                                model.evaluate(&features),
                                &features,
                            );
                            if diagnostics {
                                eprintln!(
                                    "{}",
                                    serde_json::json!({"diagnostic": "match", "word": model.word, "utterance_ms": utterance.len() * 1000 / engine::RATE, "result": result})
                                );
                            }
                            if result.detected {
                                last_detection.insert(model.word.clone(), Instant::now());
                                hub.publish(Event {
                                    event: "detected".into(),
                                    event_id: models.feedback.record_with_features(
                                        &model.word,
                                        result.distance.unwrap(),
                                        &features,
                                    )?,
                                    word: model.word.clone(),
                                    ts: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64(),
                                    score: result.score,
                                });
                            }
                        }
                    }
                }
                pending.drain(..consumed);
                if diagnostics && last_diagnostic.elapsed() >= Duration::from_secs(1) {
                    eprintln!(
                        "{}",
                        serde_json::json!({"diagnostic": "audio", "peak_frame_rms": peak_rms, "vad": segmenter.status()})
                    );
                    peak_rms = 0.0;
                    last_diagnostic = Instant::now();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ensure!(
                last_audio.elapsed() < Duration::from_secs(5),
                "microphone produced no audio for five seconds"
            ),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn shutdown_signal() -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} }
    Ok(())
}

async fn serve(options: ServeOptions) -> Result<()> {
    let ServeOptions {
        model: paths,
        socket,
        websocket,
        websocket_origin,
        device,
        queue_capacity: capacity,
        model_cache,
        voices,
        negative: negatives,
        diagnostics,
    } = options;
    ensure!(paths.len() <= 16, "at most 16 wake-word models");
    let models = paths
        .iter()
        .map(|p| load_model(p))
        .collect::<Result<Vec<_>>>()?;
    let cache_path = model_cache
        .map(Ok)
        .unwrap_or_else(hark::enrollment::default_cache_directory)?;
    let feedback = Arc::new(hark::feedback::Feedback::new(Some(
        cache_path.join("feedback"),
    ))?);
    let cache = ModelCache::new(cache_path, SynthesisSettings::new(voices, negatives)?);
    let path = socket.map(Ok).unwrap_or_else(default_socket)?;
    let (listener, _guard) = server::bind(&path)?;
    let websocket_listener = match websocket {
        Some(address) => Some(tokio::net::TcpListener::bind(address).await?),
        None => None,
    };
    let websocket_address = websocket_listener
        .as_ref()
        .map(|listener| listener.local_addr())
        .transpose()?;
    let hub = Hub::default();
    let stop = Arc::new(AtomicBool::new(false));
    let builder_stop = stop.clone();
    let (models, model_worker) = Registry::start_with_feedback(
        models,
        Arc::new(move |word| cache.get_or_build(word, &builder_stop)),
        stop.clone(),
        feedback,
    )?;
    let (done_tx, mut done_rx) = oneshot::channel();
    let worker_stop = stop.clone();
    let worker_hub = hub.clone();
    let worker_models = models.clone();
    let worker = std::thread::spawn(move || {
        let _ = done_tx.send(detect(
            worker_models,
            device,
            worker_hub,
            worker_stop,
            diagnostics,
        ));
    });
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut servers = tokio::task::JoinSet::new();
    servers.spawn(server::serve(
        listener,
        hub.clone(),
        models.clone(),
        capacity as usize,
        shutdown_rx.clone(),
    ));
    if let Some(listener) = websocket_listener {
        eprintln!("listening on ws://{}/ws", websocket_address.unwrap());
        servers.spawn(hark::websocket::serve(
            listener,
            hub,
            models,
            capacity as usize,
            hark::websocket::Options {
                allowed_origins: websocket_origin,
            },
            shutdown_rx,
        ));
    }
    eprintln!("listening on {}", path.display());
    let mut result = tokio::select! {
        result = shutdown_signal() => result,
        result = &mut done_rx => result.context("audio worker terminated").and_then(|r| r),
        result = servers.join_next() => result.context("no server tasks").and_then(|r| r.context("server task failed")).and_then(|r| r),
    };
    stop.store(true, Ordering::Relaxed);
    let _ = shutdown_tx.send(true);
    drop(model_worker);
    while let Some(server_result) = servers.join_next().await {
        let server_result = server_result
            .context("server shutdown failed")
            .and_then(|r| r);
        if result.is_ok() {
            result = server_result;
        }
    }
    if worker.join().is_err() {
        bail!("audio worker panicked");
    }
    result
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Action::Synthesize {
            word,
            text,
            output,
            voices,
            negative,
        } => synthesize_model(word, text, output, voices, negative),
        Action::Train {
            word,
            positive,
            negative,
            output,
        } => {
            let positive = positive
                .iter()
                .map(|p| audio::read_wav(p))
                .collect::<Result<Vec<_>>>()?;
            let negative = negative
                .iter()
                .map(|p| audio::read_wav(p))
                .collect::<Result<Vec<_>>>()?;
            save_model(&Model::train(word, &positive, &negative)?, &output)
        }
        Action::Check {
            model,
            wav,
            streaming,
        } => {
            let model = load_model(&model)?;
            let samples = audio::read_wav(&wav)?;
            if streaming {
                let mut segmenter = Segmenter::default();
                let mut input = vec![0.0; engine::RATE * 2];
                input.extend(samples);
                input.extend(vec![0.0; engine::RATE]);
                let results: Vec<_> = input
                    .as_chunks::<{ engine::HOP }>()
                    .0
                    .iter()
                    .filter_map(|frame| segmenter.push(frame))
                    .map(|utterance| model.evaluate(&engine::features(&utterance)))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({"word": model.word, "segments": results})
                );
                return Ok(());
            }
            let (detected, score) = model.compare(&engine::features_for_clip(&samples));
            println!(
                "{}",
                serde_json::json!({"word": model.word, "detected": detected, "score": score})
            );
            Ok(())
        }
        Action::Serve(options) => serve(options).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn websocket_bind_failure_cleans_up_uds_before_opening_microphone() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("hark.sock");
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let args = Cli::try_parse_from([
            "hark",
            "serve",
            "--socket",
            socket.to_str().unwrap(),
            "--websocket",
            &occupied.local_addr().unwrap().to_string(),
        ])
        .unwrap();
        let Action::Serve(options) = args.command else {
            unreachable!()
        };
        let error = serve(options).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::AddrInUse
        );
        assert!(!socket.exists());
    }
}
