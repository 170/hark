//! Explicit feedback tunes and protects thresholds without retaining microphone audio.
use crate::engine::{MatchResult, Model};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const HISTORY_LIMIT: usize = 256;
const HISTORY_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label {
    FalsePositive,
    TruePositive,
}

#[derive(Serialize)]
pub struct ResetReply {
    pub event: &'static str,
    pub word: String,
    pub status: &'static str,
    pub threshold: f32,
}

#[derive(Clone, Serialize)]
pub struct FeedbackReply {
    pub event: &'static str,
    pub event_id: String,
    pub label: Label,
    pub word: String,
    pub status: &'static str,
    pub threshold: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_report_is_pruned_without_new_detections() {
        let feedback = Feedback::new(None).unwrap();
        let id = feedback.record("hello", 0.08);
        feedback.state.lock().unwrap().history[0].created = Instant::now() - HISTORY_TTL;
        let error = feedback
            .apply(&id, &HashSet::from(["hello".into()]))
            .err()
            .unwrap();
        assert!(error.to_string().contains("expired"));
        assert!(feedback.state.lock().unwrap().history.is_empty());
    }
}

struct Detection {
    id: String,
    word: String,
    distance: f32,
    created: Instant,
    reply: Option<FeedbackReply>,
}

struct TunedModel {
    base: Arc<Model>,
    threshold: f32,
    positive_distance: Option<f32>,
}

#[derive(Default)]
struct State {
    models: HashMap<String, TunedModel>,
    history: VecDeque<Detection>,
    sequence: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedThreshold {
    version: u32,
    // Exact baseline comparison prevents applying feedback to a different model.
    base: Model,
    threshold: f32,
    #[serde(default)]
    positive_distance: Option<f32>,
}

pub struct Feedback {
    directory: Option<PathBuf>,
    session: String,
    state: Mutex<State>,
    writer: Mutex<()>,
}

fn floor(model: &Model) -> f32 {
    (model.calibration.positive_max_distance + 0.02).max(0.01)
}

impl Feedback {
    pub fn new(directory: Option<PathBuf>) -> Result<Self> {
        let mut nonce = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        Ok(Self {
            directory,
            session: nonce.iter().map(|b| format!("{b:02x}")).collect(),
            state: Mutex::new(State::default()),
            writer: Mutex::new(()),
        })
    }

    fn path(&self, word: &str) -> Option<PathBuf> {
        self.directory.as_ref().map(|directory| {
            let key: String = word.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
            let (prefix, suffix) = key.split_at(key.len().min(128));
            directory
                .join("v1")
                .join(prefix)
                .join(format!("{suffix}threshold.json"))
        })
    }

    /// Called by startup or the enrollment worker, never by the audio thread.
    pub fn register(&self, model: Arc<Model>) -> Result<()> {
        let mut threshold = model.threshold;
        let mut positive_distance = None;
        if let Some(path) = self.path(&model.word) {
            match File::open(&path) {
                Ok(file) => {
                    ensure!(
                        file.metadata()?.len() <= 8 * 1024 * 1024,
                        "feedback file is too large"
                    );
                    let saved: SavedThreshold = serde_json::from_reader(file)
                        .with_context(|| format!("read feedback {}", path.display()))?;
                    ensure!(
                        matches!(saved.version, 1 | 2),
                        "unsupported feedback version"
                    );
                    if saved.base == *model {
                        ensure!(
                            model.calibration.positive_max_distance.is_finite()
                                && saved.threshold.is_finite()
                                && (saved.threshold == model.threshold
                                    || saved.threshold >= floor(&model))
                                && saved.threshold <= model.threshold,
                            "invalid saved feedback threshold"
                        );
                        ensure!(
                            saved
                                .positive_distance
                                .is_none_or(|d| d.is_finite() && d >= 0.0 && d <= saved.threshold),
                            "invalid saved positive distance"
                        );
                        threshold = saved.threshold;
                        positive_distance = saved.positive_distance;
                    } else {
                        eprintln!("ignoring feedback for changed model word={:?}", model.word);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.state.lock().unwrap().models.insert(
            model.word.clone(),
            TunedModel {
                base: model,
                threshold,
                positive_distance,
            },
        );
        Ok(())
    }

    /// Only a short memory lock is taken in the detection path.
    pub fn adjust(&self, word: &str, mut result: MatchResult) -> MatchResult {
        if let Some(model) = self.state.lock().unwrap().models.get(word) {
            result.threshold = model.threshold;
            result.detected = result.distance.is_some_and(|d| d <= model.threshold);
            result.reason = match result.distance {
                None => "duration_out_of_range",
                Some(_) if result.detected => "matched",
                Some(_) => "distance_exceeds_threshold",
            };
        }
        result
    }

    pub fn record(&self, word: &str, distance: f32) -> String {
        let mut state = self.state.lock().unwrap();
        Self::prune(&mut state);
        while state.history.len() >= HISTORY_LIMIT {
            state.history.pop_front();
        }
        state.sequence += 1;
        let id = format!("{}-{}", self.session, state.sequence);
        state.history.push_back(Detection {
            id: id.clone(),
            word: word.into(),
            distance,
            created: Instant::now(),
            reply: None,
        });
        id
    }

    fn prune(state: &mut State) {
        while state
            .history
            .front()
            .is_some_and(|event| event.created.elapsed() >= HISTORY_TTL)
        {
            state.history.pop_front();
        }
    }

    /// Run on a blocking worker. Disk I/O never holds the detector's state lock.
    pub fn apply(&self, event_id: &str, words: &HashSet<String>) -> Result<FeedbackReply> {
        self.apply_label(event_id, Label::FalsePositive, words)
    }

    pub fn apply_label(
        &self,
        event_id: &str,
        label: Label,
        words: &HashSet<String>,
    ) -> Result<FeedbackReply> {
        let _writer = self
            .writer
            .try_lock()
            .map_err(|_| anyhow::anyhow!("feedback busy; retry"))?;
        let (base, positive_distance, reply) = {
            let mut state = self.state.lock().unwrap();
            Self::prune(&mut state);
            let event = state
                .history
                .iter()
                .find(|event| event.id == event_id)
                .context("unknown or expired event_id")?;
            ensure!(words.contains(&event.word), "event word is not subscribed");
            if let Some(reply) = &event.reply {
                ensure!(
                    reply.label == label,
                    "event already reported with a different label"
                );
                return Ok(reply.clone());
            }
            let tuned = state.models.get(&event.word).context("model unavailable")?;
            let lower = floor(&tuned.base).max(tuned.positive_distance.unwrap_or(0.0));
            let mut positive_distance = tuned.positive_distance;
            let (status, threshold) = if label == Label::TruePositive {
                if event.distance > tuned.threshold {
                    // Do not silently undo an earlier negative adjustment.
                    ("needs_examples", tuned.threshold)
                } else {
                    ensure!(
                        event.distance.is_finite() && event.distance >= 0.0,
                        "invalid event distance"
                    );
                    positive_distance = Some(positive_distance.unwrap_or(0.0).max(event.distance));
                    ("recorded", tuned.threshold)
                }
            } else if event.distance > tuned.threshold {
                ("unchanged", tuned.threshold)
            } else if !tuned.base.calibration.positive_max_distance.is_finite()
                || event.distance <= lower + f32::EPSILON
            {
                ("needs_examples", tuned.threshold)
            } else {
                ("applied", lower + (event.distance - lower) * 0.5)
            };
            (
                tuned.base.clone(),
                positive_distance,
                FeedbackReply {
                    event: "feedback_result",
                    event_id: event_id.into(),
                    label,
                    word: event.word.clone(),
                    status,
                    threshold,
                },
            )
        };
        if matches!(reply.status, "applied" | "recorded") {
            if let Some(path) = self.path(&reply.word) {
                let parent = path.parent().unwrap();
                fs::create_dir_all(parent)?;
                let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
                serde_json::to_writer(
                    &mut temporary,
                    &SavedThreshold {
                        version: 2,
                        base: (*base).clone(),
                        threshold: reply.threshold,
                        positive_distance,
                    },
                )?;
                temporary.flush()?;
                temporary.as_file().sync_all()?;
                temporary.persist(path).context("save feedback threshold")?;
            }
            let mut state = self.state.lock().unwrap();
            let tuned = state.models.get_mut(&reply.word).unwrap();
            tuned.threshold = reply.threshold;
            tuned.positive_distance = positive_distance;
        }
        let mut state = self.state.lock().unwrap();
        if let Some(event) = state.history.iter_mut().find(|event| event.id == event_id) {
            event.reply = Some(reply.clone());
        }
        Ok(reply)
    }
    /// Reset only feedback state, keeping the baseline model and subscriptions.
    pub fn reset(&self, word: &str, words: &HashSet<String>) -> Result<ResetReply> {
        let _writer = self
            .writer
            .try_lock()
            .map_err(|_| anyhow::anyhow!("feedback busy; retry"))?;
        ensure!(words.contains(word), "reset word is not subscribed");
        let threshold = self
            .state
            .lock()
            .unwrap()
            .models
            .get(word)
            .context("model unavailable")?
            .base
            .threshold;
        if let Some(path) = self.path(word) {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("remove feedback state"),
            }
        }
        let mut state = self.state.lock().unwrap();
        let tuned = state.models.get_mut(word).unwrap();
        tuned.threshold = threshold;
        tuned.positive_distance = None;
        state.history.retain(|event| event.word != word);
        Ok(ResetReply {
            event: "reset_result",
            word: word.into(),
            status: "reset",
            threshold,
        })
    }
}
