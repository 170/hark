//! Deduplicates model preparation without blocking socket or audio threads.
use crate::{engine::Model, feedback::Feedback};
use anyhow::{Result, bail, ensure};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::watch;

const MAX_MODELS: usize = 16;
type Completion = watch::Receiver<Option<Result<(), String>>>;
type Builder = dyn Fn(&str) -> Result<Model> + Send + Sync;

#[derive(Default)]
struct State {
    ready: HashMap<String, Arc<Model>>,
    pending: HashMap<String, Completion>,
}

struct Job {
    word: String,
    complete: watch::Sender<Option<Result<(), String>>>,
}

#[derive(Clone)]
pub struct Registry {
    state: Arc<Mutex<State>>,
    jobs: SyncSender<Job>,
    stop: Arc<AtomicBool>,
    pub feedback: Arc<Feedback>,
}

/// Joins the owned worker on shutdown, including any cancellable TTS process.
pub struct ModelWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
impl Drop for ModelWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn validate_word(word: &str) -> Result<()> {
    ensure!(
        !word.is_empty() && word.len() <= 128 && word.trim() == word,
        "word must contain 1 to 128 UTF-8 bytes without leading or trailing whitespace"
    );
    ensure!(
        word.chars().any(char::is_alphanumeric)
            && word
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '\'')),
        "word may contain letters, numbers, spaces, hyphens, underscores, and apostrophes"
    );
    Ok(())
}

pub fn spoken_text(word: &str) -> Result<String> {
    validate_word(word)?;
    Ok(word
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" "))
}

impl Registry {
    pub fn start(
        models: Vec<Model>,
        builder: Arc<Builder>,
        stop: Arc<AtomicBool>,
    ) -> Result<(Self, ModelWorker)> {
        Self::start_with_feedback(models, builder, stop, Arc::new(Feedback::new(None)?))
    }

    pub fn start_with_feedback(
        models: Vec<Model>,
        builder: Arc<Builder>,
        stop: Arc<AtomicBool>,
        feedback: Arc<Feedback>,
    ) -> Result<(Self, ModelWorker)> {
        ensure!(models.len() <= MAX_MODELS, "at most 16 wake-word models");
        let mut initial = State::default();
        for model in models {
            model.validate()?;
            validate_word(&model.word)?;
            ensure!(
                !initial.ready.contains_key(&model.word),
                "duplicate word identifiers"
            );
            let model = Arc::new(model);
            feedback.register(model.clone())?;
            initial.ready.insert(model.word.clone(), model);
        }
        let state = Arc::new(Mutex::new(initial));
        let (jobs, rx) = mpsc::sync_channel::<Job>(MAX_MODELS);
        let registry = Self {
            state: state.clone(),
            jobs,
            stop: stop.clone(),
            feedback: feedback.clone(),
        };
        let worker_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                let job = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                eprintln!("preparing model word={:?}", job.word);
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| builder(&job.word)))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("model builder panicked")))
                        .and_then(|model| {
                            model.validate()?;
                            ensure!(
                                model.word == job.word,
                                "model word does not match subscription"
                            );
                            ensure!(!worker_stop.load(Ordering::Relaxed), "service is stopping");
                            let model = Arc::new(model);
                            feedback.register(model.clone())?;
                            Ok(model)
                        });
                let mut state = state.lock().unwrap();
                let completion = match result {
                    Ok(model) => {
                        state.ready.insert(job.word.clone(), model);
                        eprintln!("model ready word={:?}", job.word);
                        Ok(())
                    }
                    Err(error) => {
                        eprintln!("model failed word={:?}: {error:#}", job.word);
                        Err(format!("cannot prepare {:?}: {error:#}", job.word))
                    }
                };
                let _ = job.complete.send(Some(completion));
                state.pending.remove(&job.word);
            }
        });
        Ok((
            registry,
            ModelWorker {
                stop,
                handle: Some(handle),
            },
        ))
    }

    /// Short snapshot lock only; matching and synthesis happen outside it.
    pub fn snapshot(&self) -> Vec<Arc<Model>> {
        self.state.lock().unwrap().ready.values().cloned().collect()
    }

    pub async fn ensure(&self, words: &HashSet<String>) -> Result<()> {
        let mut completions = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            ensure!(!self.stop.load(Ordering::Relaxed), "service is stopping");
            for word in words {
                validate_word(word)?;
            }
            let missing = words
                .iter()
                .filter(|word| {
                    !state.ready.contains_key(*word) && !state.pending.contains_key(*word)
                })
                .count();
            ensure!(
                state.ready.len() + state.pending.len() + missing <= MAX_MODELS,
                "model limit reached (16 distinct words per service run)"
            );
            // Reserve the whole subscription before starting any new work.
            let mut ordered: Vec<_> = words.iter().collect();
            ordered.sort();
            for word in ordered {
                if state.ready.contains_key(word) {
                    continue;
                }
                if let Some(completion) = state.pending.get(word) {
                    completions.push(completion.clone());
                    continue;
                }
                let (complete, completion) = watch::channel(None);
                self.jobs
                    .try_send(Job {
                        word: word.clone(),
                        complete,
                    })
                    .map_err(|_| anyhow::anyhow!("model worker is unavailable"))?;
                state.pending.insert(word.clone(), completion.clone());
                completions.push(completion);
            }
        }
        for mut completion in completions {
            loop {
                if let Some(result) = completion.borrow().clone() {
                    result.map_err(anyhow::Error::msg)?;
                    break;
                }
                if completion.changed().await.is_err() {
                    bail!("model worker stopped");
                }
            }
        }
        Ok(())
    }
}
