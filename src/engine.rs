//! Small, local template matcher. No pretrained wake-word model is used.
use anyhow::{Result, ensure};
use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, f32::consts::PI};

pub const RATE: usize = 16_000;
pub const HOP: usize = 160;
const FRAME: usize = 400;
const BANDS: usize = 26;
const DIM: usize = 13;
pub type Features = Vec<[f32; DIM]>;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub version: u32,
    pub word: String,
    pub threshold: f32,
    pub templates: Vec<Features>,
    pub calibration: Calibration,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub method: String,
    pub positive_max_distance: f32,
    pub negative_min_distance: Option<f32>,
    pub negative_count: usize,
}

#[derive(Debug, Serialize)]
pub struct MatchResult {
    pub detected: bool,
    pub score: f32,
    pub distance: Option<f32>,
    pub threshold: f32,
    pub frames: usize,
    pub reason: &'static str,
}

impl Model {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported model version; regenerate the model"
        );
        ensure!(
            !self.word.is_empty()
                && self.word.len() <= 128
                && !self.word.chars().any(char::is_control),
            "invalid word identifier"
        );
        ensure!(
            self.threshold.is_finite() && (0.01..=0.6).contains(&self.threshold),
            "invalid threshold"
        );
        ensure!(
            (3..=64).contains(&self.templates.len()),
            "model needs 3 to 64 templates"
        );
        for template in &self.templates {
            ensure!(
                (15..=300).contains(&template.len()),
                "invalid template duration"
            );
            ensure!(
                template.iter().flatten().all(|v| v.is_finite()),
                "invalid model features"
            );
        }
        Ok(())
    }

    pub fn train(word: String, positives: &[Vec<f32>], negatives: &[Vec<f32>]) -> Result<Self> {
        let groups: Vec<_> = (0..positives.len()).collect();
        Self::train_grouped(
            word,
            positives,
            negatives,
            &groups,
            "leave-one-recording-out",
        )
    }

    pub fn train_grouped(
        word: String,
        positives: &[Vec<f32>],
        negatives: &[Vec<f32>],
        groups: &[usize],
        method: &str,
    ) -> Result<Self> {
        ensure!(
            (3..=64).contains(&positives.len()),
            "provide 3 to 64 positive recordings"
        );
        ensure!(
            groups.len() == positives.len() && groups.iter().any(|g| *g != groups[0]),
            "at least two calibration groups are required"
        );
        let templates: Vec<_> = positives.iter().map(|s| features(&trim(s))).collect();
        ensure!(
            templates.iter().all(|t| (15..=300).contains(&t.len())),
            "each positive must contain 0.2 to 3 seconds of speech"
        );
        // Synthetic enrollment excludes every recording of the same voice,
        // otherwise nearby speed variants make calibration over-optimistic.
        let positive_max = templates
            .iter()
            .enumerate()
            .map(|(i, a)| {
                templates
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| groups[*j] != groups[i])
                    .map(|(_, b)| distance(a, b))
                    .fold(f32::INFINITY, f32::min)
            })
            .fold(0.0, f32::max);
        let negative_features: Vec<_> = negatives.iter().map(|s| features(&trim(s))).collect();
        ensure!(
            negative_features
                .iter()
                .all(|t| (15..=300).contains(&t.len())),
            "each negative must contain 0.2 to 3 seconds of speech"
        );
        let negative_min = negative_features
            .iter()
            .flat_map(|a| templates.iter().map(move |b| distance(a, b)))
            .fold(f32::INFINITY, f32::min);
        let mut threshold = (positive_max * 1.35 + 0.025).clamp(0.05, 0.6);
        ensure!(
            positive_max < threshold,
            "positive examples are too inconsistent; use more representative voices/examples"
        );
        if negative_min.is_finite() {
            ensure!(
                negative_min > positive_max + 0.02,
                "positive and negative examples overlap; choose a more distinctive phrase or different samples"
            );
            threshold = threshold.min((positive_max + negative_min) * 0.5);
        }
        let model = Self {
            version: 1,
            word,
            threshold,
            templates,
            calibration: Calibration {
                method: method.into(),
                positive_max_distance: positive_max,
                negative_min_distance: negative_min.is_finite().then_some(negative_min),
                negative_count: negatives.len(),
            },
        };
        model.validate()?;
        Ok(model)
    }

    pub fn compare(&self, sample: &Features) -> (bool, f32) {
        let result = self.evaluate(sample);
        (result.detected, result.score)
    }

    pub fn evaluate(&self, sample: &Features) -> MatchResult {
        let d = self
            .templates
            .iter()
            .filter(|t| {
                let ratio = sample.len() as f32 / t.len() as f32;
                (0.5..=2.0).contains(&ratio)
            })
            .map(|t| distance(sample, t))
            .fold(f32::INFINITY, f32::min);
        MatchResult {
            detected: d <= self.threshold,
            score: (-3.0 * d).exp(),
            distance: d.is_finite().then_some(d),
            threshold: self.threshold,
            frames: sample.len(),
            reason: if !d.is_finite() {
                "duration_out_of_range"
            } else if d > self.threshold {
                "distance_exceeds_threshold"
            } else {
                "matched"
            },
        }
    }
}

pub fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|v| v * v).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// Trim only the exterior silence; preserve pauses inside a phrase.
fn trim(samples: &[f32]) -> Vec<f32> {
    let levels: Vec<_> = samples.chunks(HOP).map(rms).collect();
    let peak = levels.iter().copied().fold(0.0, f32::max);
    let gate = (peak * 0.08).max(0.003);
    let Some(start) = levels.iter().position(|v| *v > gate) else {
        return Vec::new();
    };
    let end = levels.iter().rposition(|v| *v > gate).unwrap() + 1;
    samples[start.saturating_sub(2) * HOP..((end + 2) * HOP).min(samples.len())].to_vec()
}

pub fn features_for_clip(samples: &[f32]) -> Features {
    features(&trim(samples))
}

pub fn features(samples: &[f32]) -> Features {
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(512);
    let mel = |hz: f32| 2595.0 * (1.0 + hz / 700.0).log10();
    let hz = |m: f32| 700.0 * (10.0f32.powf(m / 2595.0) - 1.0);
    let points: Vec<_> = (0..BANDS + 2)
        .map(|i| hz(mel(80.0) + (mel(7600.0) - mel(80.0)) * i as f32 / (BANDS + 1) as f32))
        .collect();
    let filters: Vec<Vec<f32>> = (0..BANDS)
        .map(|b| {
            (0..257)
                .map(|k| {
                    let f = k as f32 * RATE as f32 / 512.0;
                    ((f - points[b]) / (points[b + 1] - points[b]))
                        .min((points[b + 2] - f) / (points[b + 2] - points[b + 1]))
                        .max(0.0)
                })
                .collect()
        })
        .collect();
    let mut output = Vec::new();
    for frame in samples.windows(FRAME).step_by(HOP) {
        let mut spectrum = vec![Complex::new(0.0, 0.0); 512];
        for (i, value) in frame.iter().enumerate() {
            let emphasized = value - 0.97 * if i == 0 { 0.0 } else { frame[i - 1] };
            spectrum[i].re =
                emphasized * (0.54 - 0.46 * (2.0 * PI * i as f32 / (FRAME - 1) as f32).cos());
        }
        fft.process(&mut spectrum);
        let power: Vec<_> = spectrum[..257].iter().map(|v| v.norm_sqr()).collect();
        let log_energy: Vec<_> = filters
            .iter()
            .map(|filter| {
                filter
                    .iter()
                    .zip(&power)
                    .map(|(w, p)| w * p)
                    .sum::<f32>()
                    .max(1e-10)
                    .ln()
            })
            .collect();
        let mut cepstrum = [0.0; DIM];
        for (i, value) in cepstrum.iter_mut().enumerate() {
            // Omit coefficient zero (absolute loudness).
            *value = log_energy
                .iter()
                .enumerate()
                .map(|(j, e)| e * (PI * (i + 1) as f32 * (j as f32 + 0.5) / BANDS as f32).cos())
                .sum();
        }
        let norm = cepstrum.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
        cepstrum.iter_mut().for_each(|v| *v /= norm);
        output.push(cepstrum);
    }
    output
}

/// Dynamic time warping compensates for different speaking speeds.
pub fn distance(a: &Features, b: &Features) -> f32 {
    if a.is_empty() || b.is_empty() {
        return f32::INFINITY;
    }
    let mut previous = vec![f32::INFINITY; b.len() + 1];
    previous[0] = 0.0;
    for x in a {
        let mut current = vec![f32::INFINITY; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            let cost = x.iter().zip(y).map(|(u, v)| (u - v).powi(2)).sum::<f32>();
            current[j + 1] = cost + previous[j].min(previous[j + 1]).min(current[j]);
        }
        previous = current;
    }
    previous[b.len()] / (a.len() + b.len()) as f32
}

/// Adaptive endpoint detector. Low-percentile history tracks background noise
/// without treating occasional speech as a new training label.
pub struct Segmenter {
    history: VecDeque<f32>,
    lead: VecDeque<f32>,
    active: Vec<f32>,
    quiet: usize,
    suppress: bool,
    frames: usize,
    noise: f32,
}

#[derive(Debug, Serialize)]
pub struct SegmenterStatus {
    pub noise_rms: f32,
    pub start_threshold: f32,
    pub active_ms: usize,
    pub calibrating: bool,
    pub suppressing: bool,
}
impl Default for Segmenter {
    fn default() -> Self {
        Self {
            history: VecDeque::new(),
            lead: VecDeque::new(),
            active: Vec::new(),
            quiet: 0,
            suppress: false,
            frames: 0,
            noise: 0.003,
        }
    }
}
impl Segmenter {
    pub fn status(&self) -> SegmenterStatus {
        SegmenterStatus {
            noise_rms: self.noise,
            start_threshold: (self.noise * 3.0).max(0.006),
            active_ms: self.active.len() * 1000 / RATE,
            calibrating: self.frames <= 100,
            suppressing: self.suppress,
        }
    }
    pub fn push(&mut self, frame: &[f32]) -> Option<Vec<f32>> {
        assert_eq!(frame.len(), HOP);
        let level = rms(frame);
        self.history.push_back(level);
        if self.history.len() > 500 {
            self.history.pop_front();
        }
        self.frames += 1;
        if self.frames.is_multiple_of(50) {
            let mut levels: Vec<_> = self.history.iter().copied().collect();
            levels.sort_by(f32::total_cmp);
            self.noise = levels[levels.len() / 5].clamp(0.0001, 0.05);
        }
        let voiced = level > (self.noise * 3.0).max(0.006);
        if self.frames <= 100 {
            return None;
        } // One second of background calibration.
        if self.suppress {
            self.quiet = if voiced { 0 } else { self.quiet + 1 };
            if self.quiet >= 25 {
                self.suppress = false;
                self.quiet = 0;
            }
            return None;
        }
        if self.active.is_empty() {
            if !voiced {
                self.lead.extend(frame);
                while self.lead.len() > 5 * HOP {
                    self.lead.pop_front();
                }
                return None;
            }
            self.active.extend(self.lead.drain(..));
        }
        self.active.extend(frame);
        self.quiet = if voiced { 0 } else { self.quiet + 1 };
        if self.active.len() >= 3 * RATE {
            self.active.clear();
            self.suppress = true;
            self.quiet = 0;
            return None;
        }
        if self.quiet >= 25 {
            self.quiet = 0;
            let utterance = trim(&std::mem::take(&mut self.active));
            if utterance.len() >= RATE / 5 {
                return Some(utterance);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone(frequency: f32) -> Vec<f32> {
        (0..RATE)
            .map(|i| 0.2 * (2.0 * PI * frequency * i as f32 / RATE as f32).sin())
            .collect()
    }
    #[test]
    fn matcher_distinguishes_frequency_and_tolerates_gain() {
        let a = tone(300.0);
        let b = tone(1800.0);
        let model = Model::train(
            "test".into(),
            &[a.clone(), a.clone(), a.clone()],
            std::slice::from_ref(&b),
        )
        .unwrap();
        assert!(
            model
                .compare(&features(&a.iter().map(|v| v * 0.5).collect::<Vec<_>>()))
                .0
        );
        assert!(!model.compare(&features(&b)).0);
    }
    #[test]
    fn overlapping_negatives_are_rejected() {
        let a = tone(300.0);
        assert!(Model::train("test".into(), &[a.clone(), a.clone(), a.clone()], &[a]).is_err());
    }

    #[test]
    fn silence_and_incomplete_calibration_groups_are_rejected() {
        let silence = vec![0.0; RATE];
        assert!(
            Model::train(
                "test".into(),
                &[silence.clone(), silence.clone(), silence],
                &[]
            )
            .is_err()
        );
        let a = tone(300.0);
        assert!(
            Model::train_grouped(
                "test".into(),
                &[a.clone(), a.clone(), a],
                &[],
                &[0, 0, 0],
                "test"
            )
            .is_err()
        );
    }
    #[test]
    fn endpoint_and_continuous_noise_are_bounded() {
        let mut segmenter = Segmenter::default();
        for _ in 0..150 {
            assert!(segmenter.push(&[0.001; HOP]).is_none());
        }
        for _ in 0..50 {
            assert!(segmenter.push(&[0.1; HOP]).is_none());
        }
        let mut segments = 0;
        for _ in 0..100 {
            if segmenter.push(&[0.001; HOP]).is_some() {
                segments += 1;
            }
        }
        assert_eq!(segments, 1);
        for _ in 0..1000 {
            segmenter.push(&[0.1; HOP]);
            assert!(segmenter.active.len() <= 3 * RATE);
        }
    }
}
