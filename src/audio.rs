use crate::engine::RATE;
use anyhow::{Context, Result, bail, ensure};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    f64::consts::PI,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
};

/// Windowed-sinc converter with retained history across callback boundaries.
pub struct Resampler {
    buffer: Vec<f32>,
    position: f64,
    step: f64,
    cutoff: f64,
}
impl Resampler {
    pub fn new(rate: u32) -> Self {
        Self {
            buffer: vec![0.0; 32],
            position: 32.0,
            step: rate as f64 / RATE as f64,
            cutoff: (RATE as f64 / rate as f64).min(1.0) * 0.45,
        }
    }
    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        self.buffer.extend_from_slice(input);
        let mut output = Vec::new();
        while self.position + 32.0 < self.buffer.len() as f64 {
            let center = self.position.floor() as isize;
            let (mut sum, mut weights) = (0.0, 0.0);
            for index in center - 31..=center + 32 {
                let offset = index as f64 - self.position;
                let x = 2.0 * self.cutoff * offset;
                let sinc = if x.abs() < 1e-9 {
                    1.0
                } else {
                    (PI * x).sin() / (PI * x)
                };
                let weight = sinc * (0.5 + 0.5 * (PI * offset / 32.0).cos());
                sum += self.buffer[index as usize] as f64 * weight;
                weights += weight;
            }
            output.push((sum / weights) as f32);
            self.position += self.step;
        }
        let consumed = (self.position.floor() as usize)
            .saturating_sub(32)
            .min(self.buffer.len().saturating_sub(32));
        self.buffer.drain(..consumed);
        self.position -= consumed as f64;
        output
    }
}

pub fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("read WAV {}", path.display()))?;
    let spec = reader.spec();
    ensure!(
        spec.channels > 0 && (8_000..=192_000).contains(&spec.sample_rate),
        "unsupported WAV channels/sample rate"
    );
    ensure!(
        reader.duration() <= spec.sample_rate * 30,
        "WAV exceeds 30 seconds"
    );
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            ensure!(
                (1..=32).contains(&spec.bits_per_sample),
                "invalid sample depth"
            );
            let scale = 2f32.powi(spec.bits_per_sample as i32 - 1);
            reader
                .samples::<i32>()
                .map(|v| v.map(|s| s as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    ensure!(
        samples.iter().all(|v| v.is_finite()),
        "WAV contains non-finite samples"
    );
    let mut mono: Vec<_> = samples
        .chunks_exact(spec.channels as usize)
        .map(|c| c.iter().sum::<f32>() / spec.channels as f32)
        .collect();
    mono.extend([0.0; 64]);
    Ok(Resampler::new(spec.sample_rate).push(&mono))
}

pub struct Microphone {
    pub stream: cpal::Stream,
    pub samples: Receiver<Vec<f32>>,
    pub failed: Arc<AtomicBool>,
    pub overflow: Arc<AtomicBool>,
    pub rate: u32,
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: SyncSender<Vec<f32>>,
    failed: Arc<AtomicBool>,
    overflow: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            let mono = data
                .chunks_exact(channels)
                .map(|frame| {
                    frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32
                })
                .collect();
            if tx.try_send(mono).is_err() {
                overflow.store(true, Ordering::Relaxed);
            }
        },
        move |error| {
            eprintln!("microphone error: {error}");
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )?)
}

pub fn open_microphone(name: Option<&str>) -> Result<Microphone> {
    let host = cpal::default_host();
    let device = match name {
        Some(name) => host
            .input_devices()?
            .find(|d| d.name().is_ok_and(|n| n == name))
            .context("input device not found")?,
        None => host
            .default_input_device()
            .context("no default microphone")?,
    };
    eprintln!("microphone: {}", device.name()?);
    let supported = device.default_input_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    ensure!(
        config.channels > 0 && (8_000..=192_000).contains(&config.sample_rate.0),
        "unsupported microphone configuration"
    );
    let (tx, samples) = sync_channel(32);
    let failed = Arc::new(AtomicBool::new(false));
    let overflow = Arc::new(AtomicBool::new(false));
    let args = (failed.clone(), overflow.clone());
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, tx, args.0, args.1)?,
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, tx, args.0, args.1)?,
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, tx, args.0, args.1)?,
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, tx, args.0, args.1)?,
        _ => bail!("unsupported microphone sample format {format:?}"),
    };
    stream
        .play()
        .context("start microphone; on macOS check microphone permission")?;
    Ok(Microphone {
        stream,
        samples,
        failed,
        overflow,
        rate: config.sample_rate.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resampling_is_independent_of_chunk_boundaries() {
        let input: Vec<_> = (0..4800).map(|i| (i as f32 * 0.1).sin()).collect();
        let whole = Resampler::new(48_000).push(&input);
        let mut converter = Resampler::new(48_000);
        let split: Vec<_> = input.chunks(127).flat_map(|c| converter.push(c)).collect();
        assert_eq!(whole, split);
        assert!((whole.len() as isize - 1600).abs() < 20);
    }
    #[test]
    fn downsampling_rejects_above_nyquist_tone() {
        let input: Vec<_> = (0..48000)
            .map(|i| (2.0 * PI * 12000.0 * i as f64 / 48000.0).sin() as f32)
            .collect();
        let output = Resampler::new(48000).push(&input);
        assert!(crate::engine::rms(&output[100..]) < 0.02);
    }
}
