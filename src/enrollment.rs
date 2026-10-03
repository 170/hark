use crate::{audio, engine::Model};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub fn load_model(path: &Path) -> Result<Model> {
    ensure!(
        fs::metadata(path)?.len() <= 8 * 1024 * 1024,
        "model is too large"
    );
    let model: Model = serde_json::from_slice(&fs::read(path)?)?;
    model.validate()?;
    Ok(model)
}

pub fn save_model(model: &Model, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(model)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("create model (existing files are not overwritten)")?;
    file.write_all(&json)?;
    file.sync_all()?;
    eprintln!(
        "saved {}: {} templates, threshold={:.4}, negative examples={}",
        path.display(),
        model.templates.len(),
        model.threshold,
        model.calibration.negative_count
    );
    if model.calibration.negative_count == 0 {
        eprintln!(
            "No negative examples: false-positive rejection is not calibrated. Add --negative phrases or WAVs."
        );
    }
    Ok(())
}

fn synthesize(
    text: &str,
    voice: &str,
    speed: u16,
    path: &Path,
    stop: &AtomicBool,
    deadline: Instant,
) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "model generation cancelled");
    ensure!(Instant::now() < deadline, "model generation timed out");
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("say");
        command
            .args([
                "-v",
                voice,
                "-r",
                &speed.to_string(),
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
            .args(["-v", voice, "-s", &speed.to_string(), "--stdin", "-w"])
            .arg(path);
        command
    };
    // Feed text through stdin so phrases can never become command-line options.
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("start local TTS (macOS: say; Linux: install espeak-ng)")?;
    let write_result = child
        .stdin
        .take()
        .context("TTS stdin missing")?
        .write_all(text.as_bytes());
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.into());
    }
    let status = wait_for_child(&mut child, stop, deadline)?;
    ensure!(
        status.success(),
        "TTS failed for voice {voice}; check installed voices"
    );
    Ok(())
}

fn wait_for_child(child: &mut Child, stop: &AtomicBool, deadline: Instant) -> Result<ExitStatus> {
    loop {
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("model generation cancelled or timed out");
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthesisSettings {
    voices: Vec<String>,
    negatives: Vec<String>,
}

impl SynthesisSettings {
    pub fn new(voices: Vec<String>, negatives: Vec<String>) -> Result<Self> {
        ensure!(
            voices.is_empty() || (2..=8).contains(&voices.len()),
            "select 2 to 8 different voices, or omit --voices for automatic selection"
        );
        ensure!(
            voices.iter().all(|v| !v.trim().is_empty())
                && voices.iter().collect::<HashSet<_>>().len() == voices.len(),
            "voices must be nonempty and distinct"
        );
        ensure!(
            negatives.len() <= 16
                && negatives
                    .iter()
                    .all(|s| !s.trim().is_empty() && s.len() <= 256),
            "provide at most 16 short negative phrases"
        );
        Ok(Self { voices, negatives })
    }

    fn resolve_for(&self, text: &str, stop: &AtomicBool) -> Result<Self> {
        if !self.voices.is_empty() {
            return Ok(self.clone());
        }
        let voices = if uses_japanese_script(text) {
            #[cfg(target_os = "macos")]
            {
                let mut command = Command::new("say");
                command.args(["-v", "?"]);
                japanese_mac_voices(&voice_catalog(&mut command, stop)?)?
            }
            #[cfg(target_os = "linux")]
            {
                ensure!(
                    !text.chars().any(is_han),
                    "eSpeak NG Japanese synthesis requires hiragana or katakana; provide a kana phrase or preload a model with a kana --text"
                );
                let mut command = Command::new("espeak-ng");
                command.arg("--voices");
                ensure!(
                    has_espeak_japanese(&voice_catalog(&mut command, stop)?),
                    "Japanese voice is unavailable; install an espeak-ng build with ja support and check espeak-ng --voices"
                );
                vec!["ja".into(), "ja+f3".into(), "ja+m3".into()]
            }
        } else {
            #[cfg(target_os = "macos")]
            {
                vec!["Samantha".into(), "Daniel".into(), "Karen".into()]
            }
            #[cfg(target_os = "linux")]
            {
                vec!["en-us".into(), "en-gb".into(), "en-sc".into()]
            }
        };
        Self::new(voices, self.negatives.clone())
    }
}

fn is_han(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' | '\u{20000}'..='\u{323af}')
}

fn uses_japanese_script(text: &str) -> bool {
    text.chars().any(|c| is_han(c) || matches!(c, '\u{3040}'..='\u{30ff}' | '\u{31f0}'..='\u{31ff}' | '\u{ff66}'..='\u{ff9f}'))
}

fn voice_catalog(command: &mut Command, stop: &AtomicBool) -> Result<String> {
    // A file avoids filling a stdout pipe while polling the cancellable child.
    let output = tempfile::NamedTempFile::new()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(output.reopen()?)
        .spawn()
        .context("list local TTS voices")?;
    let status = wait_for_child(&mut child, stop, Instant::now() + Duration::from_secs(10))?;
    ensure!(status.success(), "cannot list local TTS voices");
    ensure!(
        output.as_file().metadata()?.len() <= 1024 * 1024,
        "TTS voice list is too large"
    );
    Ok(fs::read_to_string(output.path())?)
}

#[cfg(any(target_os = "macos", test))]
fn japanese_mac_voices(catalog: &str) -> Result<Vec<String>> {
    let mut voices: Vec<String> = catalog
        .lines()
        .filter_map(|line| {
            let description = line.split('#').next()?.trim();
            let (name, locale) = description.rsplit_once(char::is_whitespace)?;
            (locale == "ja_JP").then(|| name.trim().to_string())
        })
        .collect();
    voices.sort();
    voices.dedup();
    let preferred = [
        "Kyoko",
        "Otoya",
        "Eddy (Japanese (Japan))",
        "Flo (Japanese (Japan))",
    ];
    voices.sort_by_key(|name| {
        preferred
            .iter()
            .position(|p| {
                *p == name
                    || name
                        .strip_prefix(p)
                        .is_some_and(|suffix| suffix.starts_with(" ("))
            })
            .unwrap_or(preferred.len())
    });
    voices.truncate(3);
    ensure!(
        voices.len() >= 2,
        "at least two installed Japanese voices are required; check say -v '?' and install Japanese voices or pass --voices explicitly"
    );
    Ok(voices)
}

#[cfg(any(target_os = "linux", test))]
fn has_espeak_japanese(catalog: &str) -> bool {
    catalog
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some("ja"))
}

fn read_synthetic_clip(path: &Path, voice: &str, speed: u16, text: &str) -> Result<Vec<f32>> {
    let samples = audio::read_wav(path).with_context(|| {
        format!("read synthesized audio: voice={voice:?}, speed={speed}, text={text:?}")
    })?;
    let frames = crate::engine::features_for_clip(&samples).len();
    ensure!(
        (15..=300).contains(&frames),
        "TTS voice {voice:?} at speed {speed} produced {frames} usable speech frames for {text:?}; expected 0.2 to 3 seconds of speech. Check that the voice supports this language (--voices) and use a short phrase"
    );
    Ok(samples)
}

fn generate(
    word: &str,
    text: &str,
    samples_dir: &Path,
    settings: &SynthesisSettings,
    stop: &AtomicBool,
) -> Result<Model> {
    ensure!(
        !text.trim().is_empty() && text.len() <= 256,
        "text must contain 1 to 256 bytes"
    );
    let deadline = Instant::now() + Duration::from_secs(180);
    let (mut positives, mut negative_audio) = (Vec::new(), Vec::new());
    for (index, voice) in settings.voices.iter().enumerate() {
        for speed in [140, 175, 210] {
            let path = samples_dir.join(format!("positive-{index}-{speed}.wav"));
            eprintln!("generating voice={voice} speed={speed}");
            synthesize(text, voice, speed, &path, stop, deadline)?;
            positives.push(read_synthetic_clip(&path, voice, speed, text)?);
        }
        for (n, phrase) in settings.negatives.iter().enumerate() {
            let path = samples_dir.join(format!("negative-{index}-{n}.wav"));
            synthesize(phrase, voice, 175, &path, stop, deadline)?;
            negative_audio.push(read_synthetic_clip(&path, voice, 175, phrase)?);
        }
    }
    fs::write(
        samples_dir.join("manifest.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"text": text, "voices": settings.voices, "speeds": [140, 175, 210], "negative_phrases": settings.negatives}),
        )?,
    )?;
    ensure!(!stop.load(Ordering::Relaxed), "model generation cancelled");
    let groups: Vec<_> = (0..positives.len()).map(|i| i / 3).collect();
    Model::train_grouped(
        word.into(),
        &positives,
        &negative_audio,
        &groups,
        "leave-one-voice-out",
    )
}

pub fn synthesize_model(
    word: String,
    text: String,
    output: PathBuf,
    voices: Vec<String>,
    negatives: Vec<String>,
) -> Result<()> {
    ensure!(!output.exists(), "output model already exists");
    let settings =
        SynthesisSettings::new(voices, negatives)?.resolve_for(&text, &AtomicBool::new(false))?;
    let samples_dir = output.with_extension("samples");
    if let Some(parent) = samples_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&samples_dir).context(
        "sample directory already exists or cannot be created; choose a new output name",
    )?;
    save_model(
        &generate(
            &word,
            &text,
            &samples_dir,
            &settings,
            &AtomicBool::new(false),
        )?,
        &output,
    )
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    version: u32,
    text: String,
    settings: SynthesisSettings,
    model: Model,
}

pub struct ModelCache {
    directory: PathBuf,
    settings: SynthesisSettings,
}
impl ModelCache {
    pub fn new(directory: PathBuf, settings: SynthesisSettings) -> Self {
        Self {
            directory,
            settings,
        }
    }

    fn path(&self, word: &str) -> PathBuf {
        // Lossless encoding prevents traversal and hash collisions. Split long
        // names so each component remains below the filesystem's name limit.
        let key: String = word.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let (prefix, suffix) = key.split_at(key.len().min(128));
        self.directory
            .join("v1")
            .join(prefix)
            .join(format!("{suffix}model.json"))
    }

    pub fn get_or_build(&self, word: &str, stop: &AtomicBool) -> Result<Model> {
        let text = crate::registry::spoken_text(word)?;
        // Cache the resolved voices, not an empty "auto" setting, so changes
        // in language selection or installed voices invalidate stale entries.
        let resolved = Self::new(
            self.directory.clone(),
            self.settings.resolve_for(&text, stop)?,
        );
        resolved.get_with(word, &text, stop, |directory| {
            generate(word, &text, directory, &resolved.settings, stop)
        })
    }

    fn get_with(
        &self,
        word: &str,
        text: &str,
        stop: &AtomicBool,
        build: impl FnOnce(&Path) -> Result<Model>,
    ) -> Result<Model> {
        ensure!(!stop.load(Ordering::Relaxed), "model generation cancelled");
        let path = self.path(word);
        if path.exists() {
            let cached = (|| -> Result<Model> {
                ensure!(
                    fs::metadata(&path)?.len() <= 8 * 1024 * 1024,
                    "cached model is too large"
                );
                let cached: CacheEntry = serde_json::from_slice(&fs::read(&path)?)?;
                ensure!(
                    cached.version == 1
                        && cached.text == text
                        && cached.settings == self.settings
                        && cached.model.word == word,
                    "cache settings changed"
                );
                cached.model.validate()?;
                Ok(cached.model)
            })();
            match cached {
                Ok(model) => {
                    eprintln!("loaded cached model word={word:?}");
                    return Ok(model);
                }
                Err(error) => eprintln!("rebuilding cache word={word:?}: {error:#}"),
            }
        }
        let parent = path.parent().context("cache path has no parent")?;
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let samples = tempfile::Builder::new()
            .prefix(".build-")
            .tempdir_in(parent)?;
        let model = build(samples.path())?;
        model.validate()?;
        ensure!(model.word == word, "generated model has the wrong word");
        ensure!(!stop.load(Ordering::Relaxed), "model generation cancelled");
        if model.calibration.negative_count == 0 {
            eprintln!(
                "word={word:?}: no negative examples; false-positive rejection is not calibrated"
            );
        }
        let entry = CacheEntry {
            version: 1,
            text: text.into(),
            settings: self.settings.clone(),
            model,
        };
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, &entry)?;
        file.as_file().sync_all()?;
        file.persist(&path)
            .context("save cached model atomically")?;
        Ok(entry.model)
    }
}

pub fn default_cache_directory() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME").filter(|p| Path::new(p).is_absolute()) {
        return Ok(PathBuf::from(path).join("hark/models"));
    }
    let home =
        PathBuf::from(std::env::var_os("HOME").context("HOME is unset; pass --model-cache")?);
    #[cfg(target_os = "linux")]
    {
        Ok(home.join(".cache/hark/models"))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(home.join("Library/Caches/hark/models"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Calibration;

    #[test]
    fn japanese_script_selection_and_explicit_override() {
        for text in [
            "こんにちは",
            "コンピュータ",
            "起動",
            "ｺﾝﾆﾁﾊ",
            "hey コンピュータ",
        ] {
            assert!(uses_japanese_script(text));
        }
        assert!(!uses_japanese_script("hey computer"));
        let explicit =
            SynthesisSettings::new(vec!["custom one".into(), "custom two".into()], vec![]).unwrap();
        assert_eq!(
            explicit
                .resolve_for("こんにちは", &AtomicBool::new(false))
                .unwrap(),
            explicit
        );
        let automatic = SynthesisSettings::new(vec![], vec![]).unwrap();
        let english = automatic
            .resolve_for("hey computer", &AtomicBool::new(false))
            .unwrap();
        assert_eq!(english.voices.len(), 3);
        assert!(!english.voices.is_empty());
    }

    #[test]
    fn japanese_voice_catalogs_handle_names_locales_and_duplicates() {
        let catalog = "Samantha (English (US)) en_US # ja_JP is not the locale here\nFlo (Japanese (Japan)) ja_JP # hello\nEddy (Japanese (Japan)) ja_JP # hello\nKyoko             ja_JP # hello\nKyoko             ja_JP # duplicate\nOtoya             ja_JP # hello\n";
        assert_eq!(
            japanese_mac_voices(catalog).unwrap(),
            ["Kyoko", "Otoya", "Eddy (Japanese (Japan))"]
        );
        assert!(japanese_mac_voices("Kyoko ja_JP # hello\nKyoko ja_JP # duplicate").is_err());
        assert!(japanese_mac_voices("Samantha en_US # hello").is_err());
        assert!(has_espeak_japanese(
            "Pty Language Age/Gender VoiceName File\n 5 ja --/M Japanese jpx/ja"
        ));
        assert!(!has_espeak_japanese(" 5 en --/M Japanese-test gmw/en"));
    }

    #[test]
    fn localized_voice_names_still_select_diverse_preferred_voices() {
        let catalog = "Eddy (Japanese (Japan)) ja_JP # hello\nFlo (Japanese (Japan)) ja_JP # hello\nGrandma (Japanese (Japan)) ja_JP # hello\nKyoko (Japanese (Japan)) ja_JP # hello\nKyoko (Japanese (Japan)) ja_JP # duplicate\n";
        assert_eq!(
            japanese_mac_voices(catalog).unwrap(),
            [
                "Kyoko (Japanese (Japan))",
                "Eddy (Japanese (Japan))",
                "Flo (Japanese (Japan))"
            ]
        );
    }

    #[test]
    fn silent_synthesis_reports_voice_speed_and_language_hint() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("silent.wav");
        let mut wav = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..16000 {
            wav.write_sample(0i16).unwrap();
        }
        wav.finalize().unwrap();
        let error = read_synthetic_clip(&path, "Samantha", 175, "こんにちは")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Samantha")
                && error.contains("175")
                && error.contains("0 usable speech frames")
                && error.contains("supports this language")
        );
    }

    #[test]
    fn cancelling_a_voice_command_kills_and_reaps_it() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(
            wait_for_child(
                &mut child,
                &AtomicBool::new(true),
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn resolved_voice_changes_invalidate_the_cache() {
        let directory = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let english = ModelCache::new(
            directory.path().into(),
            SynthesisSettings::new(vec!["Samantha".into(), "Karen".into()], vec![]).unwrap(),
        );
        english
            .get_with("こんにちは", "こんにちは", &stop, |_| {
                Ok(model("こんにちは"))
            })
            .unwrap();
        let japanese = ModelCache::new(
            directory.path().into(),
            SynthesisSettings::new(vec!["Kyoko".into(), "Otoya".into()], vec![]).unwrap(),
        );
        let mut rebuilt = false;
        japanese
            .get_with("こんにちは", "こんにちは", &stop, |_| {
                rebuilt = true;
                Ok(model("こんにちは"))
            })
            .unwrap();
        assert!(rebuilt);
        japanese
            .get_with("こんにちは", "こんにちは", &stop, |_| {
                panic!("compatible cache must be reused")
            })
            .unwrap();
    }

    fn model(word: &str) -> Model {
        Model {
            version: 1,
            word: word.into(),
            threshold: 0.1,
            templates: vec![vec![[0.1; 13]; 15]; 3],
            calibration: Calibration {
                method: "test".into(),
                positive_max_distance: 0.0,
                negative_min_distance: None,
                negative_count: 0,
            },
        }
    }

    #[test]
    fn cache_survives_restart_and_rebuilds_for_changed_settings_or_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let settings = SynthesisSettings::new(vec![], vec![]).unwrap();
        let cache = ModelCache::new(directory.path().into(), settings.clone());
        let stop = AtomicBool::new(false);
        cache
            .get_with("hey-computer", "hey computer", &stop, |_| {
                Ok(model("hey-computer"))
            })
            .unwrap();
        let restarted = ModelCache::new(directory.path().into(), settings);
        restarted
            .get_with("hey-computer", "hey computer", &stop, |_| {
                panic!("cache should avoid synthesis")
            })
            .unwrap();
        let changed = ModelCache::new(
            directory.path().into(),
            SynthesisSettings::new(vec![], vec!["other phrase".into()]).unwrap(),
        );
        let mut rebuilt = false;
        changed
            .get_with("hey-computer", "hey computer", &stop, |_| {
                rebuilt = true;
                Ok(model("hey-computer"))
            })
            .unwrap();
        assert!(rebuilt);
        fs::write(changed.path("hey-computer"), b"incomplete JSON").unwrap();
        changed
            .get_with("hey-computer", "hey computer", &stop, |_| {
                Ok(model("hey-computer"))
            })
            .unwrap();
        changed
            .get_with("hey-computer", "hey computer", &stop, |_| {
                panic!("repaired cache should load")
            })
            .unwrap();
    }

    #[test]
    fn failed_build_preserves_old_cache_and_removes_temporary_audio() {
        let directory = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let cache = ModelCache::new(
            directory.path().into(),
            SynthesisSettings::new(vec![], vec![]).unwrap(),
        );
        cache
            .get_with("word", "word", &stop, |_| Ok(model("word")))
            .unwrap();
        let path = cache.path("word");
        let original = fs::read(&path).unwrap();
        let changed = ModelCache::new(
            directory.path().into(),
            SynthesisSettings::new(vec![], vec!["negative".into()]).unwrap(),
        );
        assert!(
            changed
                .get_with("word", "word", &stop, |samples| {
                    fs::write(samples.join("partial.wav"), b"partial audio")?;
                    anyhow::bail!("TTS failed")
                })
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert_ne!(cache.path("hey-computer"), cache.path("hey computer"));
        let long_path = cache.path(&"x".repeat(128));
        assert!(long_path.components().all(|c| c.as_os_str().len() < 256));
    }
}
