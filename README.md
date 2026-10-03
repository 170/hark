# hark

hark is a Rust service that opens a microphone on a Linux or macOS host and pushes wake-word detection events to clients over a Unix domain socket (UDS) and an optional WebSocket listener. Detection runs locally, without cloud APIs or an existing wake-word engine.

**The custom detection engine is a prototype and is not ready for production use.** You can generate an initial model from multiple synthetic voices without recording yourself, but testing with an unseen synthetic voice has produced false positives on other phrases. Detection and false-positive rates for human speech have not been evaluated. The current implementation expects an isolated wake phrase followed by approximately 250 ms of silence. It does not support spotting keywords within continuous speech or recognizing commands spoken immediately after the wake phrase.

## Build

Use stable Rust (1.98 during development). Linux requires the ALSA development libraries.

```bash
# Debian / Ubuntu only
sudo apt-get install libasound2-dev espeak-ng

cargo build --release --locked
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Microphone capture uses [CPAL 0.16](https://docs.rs/cpal/0.16.0/cpal/). Feature extraction, matching, and automatic tuning are implemented in hark.

## Start the service and subscribe

No model needs to be prepared before starting the service:

```bash
target/release/hark serve
```

In another terminal, subscribe to a word:

```bash
# macOS
python3 examples/subscribe.py "$HOME/Library/Caches/hark/hark.sock" hey-computer
# Linux
python3 examples/subscribe.py "$XDG_RUNTIME_DIR/hark/hark.sock" hey-computer
```

A subscription such as `{"subscribe":["hey-computer"]}` automatically prepares the corresponding model. The service converts hyphens and underscores to spaces, so `hey-computer` is synthesized as "hey computer." The event's `word` remains exactly the identifier supplied by the client. Identifiers may contain letters, numbers, ordinary spaces, hyphens, underscores, and apostrophes; they must contain at least one letter or number, have no leading or trailing whitespace, and fit within 128 UTF-8 bytes. Different identifiers remain separate subscriptions even when they produce the same spoken phrase.

The first subscription generates positive examples using local TTS, calibrates a model, and adds it to the running detector. Normally three voices at three speeds produce nine examples; macOS uses six examples if only two suitable Japanese voices are available. **Wait for the `ready` event on the subscription connection before speaking.** The service also logs `subscription ready`. A subscription becomes active once all its requested models are ready; detections during preparation are not replayed. This can take several seconds or longer, especially when other models are queued.

Generation runs in a dedicated worker, one model at a time. Concurrent subscriptions to the same word share the same job. Existing models and subscriptions remain available during preparation. The service keeps at most 16 distinct models, including pending jobs, per run. Models stay loaded after clients disconnect so later subscriptions can reuse them. Disconnecting during preparation cancels the subscription, while its model may finish building for reuse. A request that would exceed the model limit is rejected before scheduling any new models.

Generated models are cached across restarts:

- macOS: `$HOME/Library/Caches/hark/models`.
- Linux: `$XDG_CACHE_HOME/hark/models`, or `$HOME/.cache/hark/models` when `XDG_CACHE_HOME` is unset or not absolute.
- Override the location with `--model-cache /path/to/cache`.

Cache entries include the word, spoken text, resolved voice names, synthesis settings, and format version. Changing the selected voices invalidates an older entry, including entries previously synthesized with voices for the wrong language. They are written atomically and reused only when compatible; incompatible or corrupt entries are rebuilt. Temporary synthesis WAVs are removed after each automatic build. Failed builds are not installed, and a new subscription can retry them. Cache entries from previous runs are retained on disk; remove unwanted entries while the service is stopped if necessary.

Set the synthesis voices and optional negative phrases when starting the service:

```bash
target/release/hark serve \
  --negative 'good morning' \
  --negative 'turn off the lights'
```

Without `--voices`, voice selection happens per phrase: hiragana, katakana (including half-width katakana), or Han characters select Japanese synthesis; other text retains the English defaults. This is a script heuristic, not general language identification: Han-only Chinese text is also routed to Japanese, and romanized Japanese is routed to English. Use `--voices 'voice1,voice2,voice3'` to override selection with two to eight installed voices. An explicit override applies to every generated word and is never replaced automatically. Automatic generation cannot infer pronunciation from opaque identifiers. Negative phrases apply to every generated word. Without them, thresholds are calibrated from positive examples only, and the service logs that false-positive rejection has not been calibrated. Changing these options takes effect after a restart and causes affected cache entries to be rebuilt on demand.

For example, on macOS you can subscribe to Japanese with the same service started using `hark serve`:

```bash
python3 examples/subscribe.py "$HOME/Library/Caches/hark/hark.sock" "こんにちは"
```

hark selects up to three installed `ja_JP` voices from `say -v '?'`, preferring Kyoko and Otoya when available before other voices. Both short names and names with locale suffixes, such as `Kyoko (Japanese (Japan))`, are recognized. This prevents preferred voices from being skipped on macOS versions that return expanded names. At least two are required; if fewer are available, the subscription error explains that Japanese voices must be installed or supplied with `--voices`. Voices are not downloaded automatically. Wait for `{"event":"ready","words":["こんにちは"]}` before speaking.

On Linux, automatic Japanese synthesis checks for eSpeak NG's `ja` voice and uses `ja`, `ja+f3`, and `ja+m3`. These are variants of one synthesis engine, not independent human speakers. eSpeak NG's Japanese support is limited to [hiragana and katakana](https://github.com/espeak-ng/espeak-ng/blob/master/docs/languages.md); automatic generation rejects Han characters on Linux with a request for a kana pronunciation. Use a kana subscription, or create a model explicitly with a kana `--text` and preload it for an identifier containing kanji. The `+variant` mechanism is described in [eSpeak NG's voice documentation](https://github.com/espeak-ng/espeak-ng/blob/master/docs/voices.md).

If synthesis produces silence or an utterance outside the supported length, the error reports the voice, speed, phrase, and usable speech frame count. A successful TTS process exit alone does not establish that usable audio was generated. For example, some English macOS voices return silence for Japanese text. The detector's duration limits are unchanged.

## Optional: create a model explicitly

The explicit synthesis and training commands remain available when an identifier needs a different pronunciation or you want to provide recordings. Preload such a model with `--model`; it takes precedence over automatic generation for that identifier.

```bash
target/release/hark synthesize \
  --word hey-computer \
  --text 'hey computer' \
  --negative 'good morning' \
  --negative 'turn off the lights' \
  --output models/hey-computer.json

target/release/hark serve --model models/hey-computer.json
```

`word` is the identifier used in events; `text` is the phrase to synthesize. Local text-to-speech (TTS) generates WAV files without playing audio through the speakers.

- macOS uses `Samantha`, `Daniel`, and `Karen` for English and selects installed `ja_JP` voices for Japanese through the built-in `say` command. Run `say -v '?'` to list available voices.
- Linux uses `en-us`, `en-gb`, and `en-sc` for English; Japanese uses `ja` with the variants described above. Run `espeak-ng --voices` to check available languages.
- Each voice is synthesized at 140, 175, and 210 words per minute, producing nine positive examples by default. Override the voices with `--voices 'voice1,voice2,voice3'`. Voice selection uses the `--text` phrase rather than the event identifier; provide the intended pronunciation, and use `--voices` only when you want an explicit override.
- `--negative` specifies a phrase that should not trigger detection. Each phrase is synthesized with every selected voice and used to limit the matching threshold. Include similar-sounding phrases to expose confusion. Model creation fails if the positive and negative examples cannot be separated. For automatic generation, this failure is returned to the subscriber.
- A sibling directory named `hey-computer.samples/` stores the WAV files and generation settings. The explicit `synthesize` command never overwrites existing models or sample directories. After a failed run, choose a new output name or remove the unwanted generated files before retrying.
- Install TTS voices beforehand. hark does not download voice models automatically.

Adding synthetic voices does not reproduce every variation in human speech, microphone distance, or room acoustics. In particular, differences between eSpeak voices should not be treated as a substitute for diverse human speakers. You can also train with WAV files from another TTS system or positive and negative recordings from the intended environment.

```bash
target/release/hark train --word hey-computer \
  --positive voice-a.wav voice-b.wav voice-c.wav \
  --negative confusing-phrase.wav \
  --output models/custom.json

# Match an isolated utterance without opening the microphone.
target/release/hark check --model models/custom.json --wav held-out.wav
```

WAV files must use integer PCM or 32-bit floating-point samples, a sample rate between 8 and 192 kHz, and a duration of at most 30 seconds. Audio is mixed to mono and resampled to 16 kHz. Each positive or negative example should contain one isolated utterance lasting approximately 0.2 to 3 seconds after trimming leading and trailing silence. Evaluating on the same audio used for training does not establish detection accuracy.

## Diagnose missed detections

Start the service with diagnostic logging:

```bash
target/release/hark serve --diagnostics
```

After the subscriber receives `ready`, say the phrase once and leave a short pause. Standard error includes two kinds of JSON diagnostic records:

- `audio` is emitted approximately once per second, with the highest 10 ms frame RMS in that interval, the estimated background noise, the speech onset threshold, and segmentation state. If `peak_frame_rms` stays below `vad.start_threshold`, speech is not crossing the onset gate. `calibrating` identifies the initial noise calibration period; `suppressing` indicates that an overlong utterance was discarded and the detector is waiting for silence.
- `match` is emitted for each eligible model when a speech segment is completed. It includes utterance duration, feature frame count, matching distance, threshold, score, and a reason: `matched`, `distance_exceeds_threshold`, or `duration_out_of_range`. A lower distance is a closer match. Distance is `null` when no template has a compatible duration. The usual one-second detection cooldown still applies.

These records distinguish microphone/segmentation problems from a model rejecting an utterance. Diagnostic logging does not record audio or transcribe speech. Default service operation remains quiet apart from lifecycle and error logs.

To inspect an isolated WAV through the same segmenter and feature extraction used during live capture:

```bash
target/release/hark check --model models/custom.json --wav held-out.wav --streaming
```

This adds two seconds of silence for calibration and one second of trailing silence, then reports matching decisions in `segments`. An empty array means no complete utterance was produced. This replay does not reproduce the acoustics or background calibration of a live microphone session. Without `--streaming`, `check` compares the trimmed clip directly.

## Custom engine and automatic tuning

Hark uses **MFCC + DTW template matching**, implemented in Rust using established signal-processing algorithms. When a word is first subscribed to, local TTS generates examples with different voices and speaking speeds. The model stores their acoustic feature sequences and a detection threshold; no neural network is trained. Microphone audio is split into utterances, converted to the same features, and compared with the templates. A detection is emitted when the closest matching distance is at or below the word's threshold.

Automatic tuning adjusts the background-noise gate and matching thresholds. False-positive feedback tightens a threshold, while confirmed-positive feedback protects known correct matches from later tightening. Neither updates the templates; feedback adjustments can be reset per word. Differences between synthetic and human speech can still cause missed detections, while a looser threshold can admit other words. Human-speech accuracy remains unverified.

1. Audio is converted to 16 kHz. Features are extracted from 25 ms windows with a 10 ms hop. Mel-frequency cepstral coefficients (MFCCs) represent the sound while reducing the influence of absolute volume.
2. Dynamic time warping (DTW) compares feature sequences while accommodating different speaking speeds. No pretrained wake-word model is used.
3. During model creation, distances between positive examples determine the matching threshold automatically. For synthetic enrollment, all speed variants of the voice being evaluated are excluded from its comparison candidates. This prevents variants of the same voice from making calibration appear overly successful. Negative examples, when provided, further constrain the threshold. Calibration results are saved in the model JSON.
4. While running, the service uses the 20th percentile of audio levels from the last five seconds to adapt speech onset detection to background noise. Remain quiet for the first second after startup. Noise estimates have upper and lower bounds. Detections are never treated as correct labels for self-training. Environmental estimates reset on restart.
5. Explicit false-positive feedback can lower the matching distance threshold for a word; true-positive feedback records a lower bound that preserves confirmed matches. This adjustment is persisted separately from the baseline model and takes effect without restarting. Silence, client timeouts, and a missing follow-up command are never interpreted as feedback.

`score` is a similarity measure computed as `exp(-3 * matching_distance)`, not a probability of correctness. Each model has its own threshold. Matching happens after an utterance ends, and repeated notifications for the same word are suppressed for one second. Utterances lasting three seconds or longer are discarded.

The queue between the microphone callback and the detector is also bounded. If capture stalls or fails, or processing cannot keep up, the service exits with an error so the service manager can restart it. It does not continue matching audio with missing chunks joined together. Live microphone audio is processed in memory and is not saved to recording files.

## Subscription protocol

The default socket is `$XDG_RUNTIME_DIR/hark/hark.sock` on Linux and `$HOME/Library/Caches/hark/hark.sock` on macOS. Override it with `--socket`.

The JSON objects below are shared by UDS and WebSocket. UDS uses UTF-8 JSON Lines; WebSocket uses one JSON object per text message, without requiring a newline. See [WebSocket connections](#websocket-connections) for listener and client examples.

For UDS, within five seconds of connecting, send exactly one UTF-8 JSON message terminated by a newline:

```json
{"subscribe":["hey-computer"]}
```

Once all requested models are available and the subscription is registered, the service sends exactly one readiness event on that connection:

```json
{"event":"ready","words":["hey-computer"]}
```

`words` contains the subscribed identifiers, deduplicated and sorted. Readiness is sent for every successful subscription, including reconnects and subscriptions using preloaded or cached models. It is always sent before any `detected` events on the same connection. Model preparation failures return an error and close the connection without a `ready` event.

Clients must dispatch messages by `event`: `ready` has `words`, `detected` has `event_id`, `word`, `ts`, and `score`, a feedback response uses `feedback_result`, and a reset response uses `reset_result`. The example subscriber prints incoming messages. Clients should allow additional event fields rather than assuming every server message is a detection.

After readiness, the service sends one line for each detection.

```json
{"event":"detected","event_id":"aa0b52749754ae02542f36c5aa922574-1","word":"hey-computer","ts":1759492560.123,"score":0.93}
```

`ts` is the UNIX timestamp, in seconds, when the service detects the phrase. Invalid JSON, invalid word identifiers, empty subscriptions, unknown fields, declarations exceeding 4096 bytes, and subscription timeouts cause an error line followed by disconnection. Valid words without a loaded model trigger automatic model preparation.

```json
{"error":"invalid_subscription","message":"subscribe must contain 1 to 64 words"}
```

A model generation failure, cache write failure, or model capacity limit returns `model_unavailable` and closes the affected connection. Other subscriptions continue to run. The five-second deadline applies only to receiving the initial subscription line, not to model preparation. TTS subprocesses share a 180-second synthesis deadline per model and are stopped when the service shuts down. Queued models wait for earlier jobs to finish.

Keep both directions of the connection open. After `ready`, clients may send feedback messages as described below. A second subscription declaration is invalid. EOF or a WebSocket close removes the subscription. The service supports up to 128 connections total across both transports (including pending handshakes and model preparation) and 64 words per subscription. Each connection has a send queue of 32 detection events by default, configurable with `--queue-capacity`; feedback responses use a separate queue with the same bound. A connection is closed if its detection queue overflows or a write stalls for five seconds. Events are neither persisted nor replayed; clients must reconnect and subscribe again after disconnection.

```bash
# Linux
python3 examples/subscribe.py "$XDG_RUNTIME_DIR/hark/hark.sock"
# macOS
python3 examples/subscribe.py "$HOME/Library/Caches/hark/hark.sock"
```

Optionally preload multiple models with `serve --model models/one.json models/two.json`. Other subscribed words are still generated automatically. New generated models become available without a restart. Restart the service to reload changes to explicit model files or synthesis settings. Capture uses the default microphone unless you specify `--device 'device name'`.

## Detection feedback and reset

A subscribing application can report a known false detection on its existing connection after `ready`:

```json
{"feedback":{"event_id":"aa0b52749754ae02542f36c5aa922574-1","label":"false_positive"}}
```

The ID identifies the actual detection, so the application does not upload audio or supply a score. The service records the peer UID for UDS, or the remote IP and port for WebSocket, when feedback is accepted. Use `false_positive` when the wake word was not spoken and `true_positive` when the detection was correct. Absence of a response is not sufficient evidence for either label.

```json
{"event":"feedback_result","event_id":"aa0b52749754ae02542f36c5aa922574-1","word":"hey-computer","label":"false_positive","status":"applied","threshold":0.04}
```

| Status | Meaning |
| --- | --- |
| `applied` | The stricter threshold has been saved and installed for subsequent matching decisions. |
| `recorded` | The confirmed positive distance has been saved to protect it from future threshold tightening. The threshold is unchanged. |
| `unchanged` | An earlier adjustment already excludes this event's distance; no further change was needed. |
| `needs_examples` | A negative overlaps the protected positive range, or a delayed positive is already excluded by a previous adjustment. No threshold or positive bound was changed. |

The detector accepts a phrase when its matching distance is at most the threshold, so **lower thresholds are stricter**. For an applicable report, the new threshold is halfway between the false detection's distance and a lower bound of `max(0.01, positive_max_distance + 0.02, confirmed_positive_distance)`. This preserves a margin over the baseline's positive calibration distances. It does **not** guarantee that every human pronunciation remains detectable: synthetic calibration is not a substitute for evaluating false negatives with real speakers. This first feedback mechanism adjusts a threshold; it does not retrain acoustic features or learn a new negative template. Similar-sounding false matches inside the positive range cannot be fixed by this adjustment alone.

Each detection has the same ID for all subscribers on both transports. Feedback affects the word globally, including other subscribers. Any connected client subscribed to that word may report a known ID; there is no separate feedback writer permission. A client may reconnect using either transport, subscribe to the word, wait for `ready`, and report an ID received earlier. Hark retains distance metadata for at most the newest 256 detections across all words, for up to ten minutes. Pending IDs are forgotten on service restart. No live microphone waveform or acoustic feature sequence is saved for feedback.

Repeated reports with the same label for a retained ID return the original result without another adjustment, including reports from different clients. A different label for an already reported ID is rejected, even if the original result was `needs_examples`. Reset the word to clear its feedback history when correcting a mistaken report. Detections already queued or being evaluated when feedback arrives can still be delivered. Continue dispatching all event types while waiting for the matching `feedback_result`.

Unknown or expired IDs, conflicting labels, unsubscribed words, a busy feedback writer, and persistence failures return `feedback_rejected` with `event_id` and `message`; the subscription remains open. Retry transient failures with the same ID. Invalid JSON, unknown fields or labels, a second subscription declaration, and lines over 4096 bytes return `invalid_feedback` and close the connection. Wait for `ready` before sending feedback.

Adjustments are stored under `<model-cache>/feedback/v1/`, including for models loaded with `--model`. Files contain the baseline model, adjusted threshold, and maximum confirmed-positive distance, and are replaced atomically before the live threshold changes. A failed save leaves both the current threshold and confirmed-positive bound unchanged. Existing version 1 files are supported; subsequent writes use version 2, which older hark binaries cannot read. On restart, an adjustment is restored only if its baseline exactly matches the loaded model; changing the model invalidates it. Corrupt feedback files cause model preparation to fail instead of silently discarding an adjustment. Stop hark and remove the model cache's `feedback` directory to reset all adjustments while keeping generated models. The standalone `check` command evaluates the baseline model only; use `serve --diagnostics` to inspect the effective live threshold.

To confirm a correct detection, send:

```json
{"feedback":{"event_id":"aa0b52749754ae02542f36c5aa922574-2","label":"true_positive"}}
```

Hark retains the largest confirmed-positive distance per word, not the recorded audio or a new acoustic template. Later negative reports cannot lower the threshold below this distance. Positive feedback does not widen the threshold, recover missed detections, or automatically undo earlier negative feedback. If another report has already lowered the threshold below a delayed positive's distance, the response is `needs_examples`; use reset if that prior adjustment was wrong.

To reset one subscribed word after `ready`, send:

```json
{"reset":{"word":"こんにちは"}}
```

The response includes the original model threshold (the number below is illustrative):

```json
{"event":"reset_result","word":"こんにちは","status":"reset","threshold":0.3}
```

Reset deletes the word's persisted feedback adjustment, clears its confirmed-positive bound and retained event IDs, and restores its baseline threshold immediately. It keeps the model, its original training-time calibration, and all subscriptions. Other words are unaffected. It is not a TTS rebuild or a reset of microphone noise estimation. The reset is shared by all clients and remains effective after restart. Already queued events may still arrive with IDs that have become invalid.

Any connected client subscribed to the word may reset it. An unsubscribed word, busy writer, or file removal failure returns `reset_rejected` and leaves live state unchanged. Send only one `feedback` or `reset` operation per message; malformed requests return `invalid_feedback` and close the connection. A repeated reset restores the baseline again, so do not retry an old reset after new feedback has been collected unless you intend to discard that feedback too.

For manual feedback, copy an `event_id` printed by the subscriber and run:

```bash
# macOS example; use "$XDG_RUNTIME_DIR/hark/hark.sock" on Linux.
python3 examples/feedback.py "$HOME/Library/Caches/hark/hark.sock" \
  'こんにちは' '<event_id from the detected event>'

# Confirm a correct detection:
python3 examples/feedback.py "$HOME/Library/Caches/hark/hark.sock" \
  'こんにちは' '<event_id from the detected event>' --label true_positive

# Reset feedback adjustments for this word:
python3 examples/feedback.py "$HOME/Library/Caches/hark/hark.sock" 'こんにちは' --reset
```

Restart hark with the updated binary before using feedback. Existing clients that ignore additional detection fields can continue subscribing without sending feedback.

## WebSocket connections

WebSocket is disabled by default. Enable it alongside UDS with an explicit bind address:

```bash
# Local host clients
target/release/hark serve --websocket 127.0.0.1:8765

# Inside a Linux container: explicitly set the UDS path if XDG_RUNTIME_DIR is unset.
target/release/hark serve --socket /run/hark/hark.sock --websocket 0.0.0.0:8765
```

The endpoint is `/ws` (for example `ws://127.0.0.1:8765/ws`). The address accepts an IPv4 or bracketed IPv6 literal and port. A bind failure stops startup; the listener is not silently skipped. Both listeners stop when hark exits.

This listener is **unauthenticated**, intended for communication on a private container network with no external publication. When hark is a service named `hark` on the same Docker network as its clients, connect to `ws://hark:8765/ws`; a host `ports` mapping is not needed for that connection. Every reachable client can subscribe and submit feedback. There is no TLS termination in hark. Microphone access is still required in the process/container running hark; this change only adds a notification transport.

The optional Python example uses the [websockets synchronous client](https://websockets.readthedocs.io/en/stable/reference/sync/client.html):

```bash
python3 -m venv .venv
.venv/bin/python -m pip install -r examples/requirements-websocket.txt
.venv/bin/python examples/subscribe_ws.py ws://127.0.0.1:8765/ws 'こんにちは'

# In a client container on the same network:
python3 examples/subscribe_ws.py ws://hark:8765/ws 'こんにちは'

# Report a known false positive and exit after feedback_result:
.venv/bin/python examples/subscribe_ws.py ws://127.0.0.1:8765/ws 'こんにちは' \
  --feedback '<event_id from the detected event>'

# Confirm a correct detection:
.venv/bin/python examples/subscribe_ws.py ws://127.0.0.1:8765/ws 'こんにちは' \
  --feedback '<event_id from the detected event>' --label true_positive

# Reset one word's feedback adjustments:
.venv/bin/python examples/subscribe_ws.py ws://127.0.0.1:8765/ws 'こんにちは' --reset
```

The subscriber reconnects and resubscribes after connection loss. Feedback and reset modes exit after acknowledgment; if disconnected before acknowledgment, retry the same event ID. Client containers need the Python requirements installed too. The example bypasses environment HTTP proxies for direct container connections.

Protocol behavior:

- Complete the HTTP upgrade within five seconds, then send `{"subscribe":["こんにちは"]}` within another five seconds. Model preparation has the same separate deadline as UDS.
- Wait for `ready`, then consume `detected` and optionally send `feedback` or `reset`. Models, generation jobs, event IDs, and feedback adjustments are shared with UDS.
- Send exactly one JSON object per UTF-8 text message. A trailing newline is accepted as JSON whitespace; multiple JSON Lines packed into one WebSocket message are invalid. Binary messages are rejected.
- Incoming text messages and frames are limited to 4096 bytes, including whitespace. Fragmented messages are reassembled within the same total limit. JSON validation errors return the same application error objects as UDS and then close; malformed WebSocket framing or size violations close the connection, with an application error when the transport still allows it.
- Ping/Pong is handled during model preparation and active subscriptions. Hark replies to client pings; clients may send pings to detect dead connections. Close, disconnect, queue overflow, and service shutdown remove subscriptions.
- WebSocket peers are logged by IP and port. Unix peer credentials are available only on UDS.

Non-browser clients normally omit `Origin` and connect without additional configuration. Browser requests with an `Origin` header are rejected unless its exact value is listed with `--websocket-origin` (repeatable), for example `--websocket-origin http://localhost:8000`. This is a browser origin check, not authentication; non-browser clients can omit or forge the header. The implementation uses [tokio-tungstenite](https://docs.rs/tokio-tungstenite/0.30.0/tokio_tungstenite/) with bounded message and write buffers.

## Socket lifecycle and permissions

The socket mode is `0660`; newly created parent directories use `0750`. The parent directory must be owned by the service user and must not be writable by the group or other users. A lock prevents duplicate instances. Only stale sockets that refuse connections are removed. Regular files, symbolic links, and active sockets are not replaced.

On SIGINT, SIGTERM, or a normal error exit, the service closes subscriptions and removes the socket. If a socket remains after SIGKILL or another abrupt exit, the next startup removes it. The directory mounted into Docker and the lock file are retained.

The service obtains the peer UID using `SO_PEERCRED` on Linux and `LOCAL_PEERCRED` on macOS, and logs it to standard error. The macOS constants and structures follow [Apple's definitions](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/un.h). UIDs are logged for auditing; access is controlled through file and directory permissions.

## Linux: systemd --user and Docker

```bash
install -Dm755 target/release/hark "$HOME/.local/bin/hark"
install -Dm644 deploy/hark.service "$HOME/.config/systemd/user/hark.service"
systemctl --user daemon-reload
systemctl --user enable --now hark
journalctl --user -u hark -f
```

The service user needs access to the host microphone. Running after logout and retaining access to audio devices depend on the distribution's session configuration. Starting a user service does not itself grant device permissions.

Bind mount **the directory containing the socket** into the container. Mounting the socket file itself leaves the container pointing at the old inode after a service restart. The example systemd unit also avoids `RuntimeDirectory`, which would otherwise remove the directory when the service stops.

```bash
# Start hark first and confirm that the host directory exists.
docker run --rm \
  --user "$(id -u):$(id -g)" \
  --mount "type=bind,src=$XDG_RUNTIME_DIR/hark,dst=/run/hark,readonly" \
  YOUR_CLIENT_IMAGE

# To retain the container's UID, add the directory and socket's group instead.
docker run --rm \
  --group-add "$(stat -c %g "$XDG_RUNTIME_DIR/hark")" \
  --mount "type=bind,src=$XDG_RUNTIME_DIR/hark,dst=/run/hark,readonly" \
  YOUR_CLIENT_IMAGE
```

Connect to `/run/hark/hark.sock` inside the container. If an existing directory has mode `0700`, its owner may need to change it to `0750`. Clients need both directory search permission and the appropriate socket permissions. When using a dedicated shared group, ensure that the directory and socket have the same GID; setting the directory's setgid bit makes new sockets inherit its group. Rootless Docker and user namespaces also require compatible host-to-container UID/GID mappings.

## Published container image

After the Linux and macOS CI checks pass, each push to `main` (including a merged
pull request) builds and publishes `ghcr.io/170/hark:latest` and
`ghcr.io/170/hark:sha-<full-commit-sha>`. Images support `linux/amd64` and
`linux/arm64` (including 64-bit Raspberry Pi OS). Pull requests build the image
without publishing it. The workflow uses `GITHUB_TOKEN` with `packages: write`;
no additional registry secret is required. After the first publication, set the
package's visibility to **Public** in GitHub's package settings to allow anonymous
pulls. Repository visibility alone does not make a new GHCR package public.

The image includes ALSA and eSpeak NG and runs as UID/GID `10001:10001`. It starts
`hark serve --socket /run/hark/hark.sock --websocket 0.0.0.0:8765`, with
the model/feedback cache under `/var/cache/hark`. For example, on a Linux host:

```bash
docker network create hark
docker volume create hark-cache
docker run -d --name hark --restart unless-stopped \
  --network hark \
  --device /dev/snd \
  --group-add "$(stat -c %g /dev/snd/controlC0)" \
  --mount type=volume,src=hark-cache,dst=/var/cache/hark \
  ghcr.io/170/hark:latest
```

Clients on the `hark` Docker network connect to `ws://hark:8765/ws`; no host port
is published. Adjust the audio device group for your host. The container needs
an available ALSA capture device; desktop audio servers may already hold it.
To select a device, append
`serve --socket /run/hark/hark.sock --websocket 0.0.0.0:8765 --device <device-name>`
to the command. Arguments after the image replace its entire default command.
Microphone access through macOS Docker Desktop is not supported by this setup.
Architecture support does not establish detection accuracy or Raspberry Pi
performance. The Linux Japanese synthesis limitations above still apply.

To build locally without opening a microphone:

```bash
docker build -t hark:local .
docker run --rm hark:local --help
```

## macOS: LaunchAgent

Replace `/Users/YOUR_USER` in `deploy/dev.hark.agent.plist` with your actual absolute paths. Install the binary, then save the plist under `~/Library/LaunchAgents/`. Neither `~` nor environment variables are expanded inside the plist.

```bash
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/dev.hark.agent.plist"
launchctl kickstart "gui/$(id -u)/dev.hark.agent"
# Stop and unregister the agent.
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/dev.hark.agent.plist"
```

Run hark as a LaunchAgent in the logged-in user's session, not as a LaunchDaemon. Microphone access requires a separate TCC permission grant. Successful capture when launched from Terminal does not establish that capture will work from a LaunchAgent. Distribution signing, application identity, and the permission request flow are not yet implemented and require verification in a real user session. Errors are written to `~/Library/Logs/hark.log`. Development work on this repository has not registered a background service or changed TCC settings.

Host processes on macOS can connect directly to the UDS. Connections from Docker Desktop containers on macOS to the host socket are out of scope.

## Validation and future work

Automated tests cover subscription validation, event filtering, cleanup after disconnection, removal of slow subscribers, socket permissions, stale socket recovery, duplicate instance prevention, peer UIDs, audio conversion, matching, noise adaptation, on-demand model installation, readiness notification ordering, shared generation jobs, cache reuse and invalidation, retry after failure, and cancellation during shutdown. Feedback tests check false-match rejection while preserving a distinct positive, positive-range protection, persistence and baseline invalidation, no live change after a failed save, bounded/expired IDs, idempotent retries across connections, confirmed-positive protection, conflicting labels, persistent reset with event invalidation, legacy file migration, and malformed message handling. WebSocket tests exercise shared UDS/WebSocket event IDs and feedback, fragmentation and size limits, ping/pong during preparation, reconnects, handshake and subscription deadlines, origin checks, bind failure cleanup, and the shared connection limit. These controlled tests do not establish human speech accuracy. CI is configured for Linux and macOS.

After installing the optional Python WebSocket requirements in the Python environment on `PATH`, test the example against a local hark WebSocket listener without opening a microphone:

```bash
PATH="$PWD/.venv/bin:$PATH" cargo test --locked --test websocket \
  python_example_waits_for_ready_and_reports_feedback -- --ignored
```

Opt-in integration tests use installed native TTS voices to exercise English and Japanese subscriptions, model generation, readiness, and event delivery without opening a microphone:

```bash
cargo test --locked --test auto_enrollment -- --ignored --nocapture
```

These tests synthesize evaluation WAVs at a speaking rate not used for training, including a voice excluded from training. They replay audio samples through speech segmentation, feature extraction, and matching at normal and reduced volume before sending a detection event. They verify the audio-to-event path, not human speech accuracy.

During investigation of missed Japanese detections, the previous model rejected a Kyoko utterance of the target phrase at a distance of approximately 0.362 against a threshold of 0.284. Expanded voice names had prevented Kyoko from being included in training. After correcting selection and rebuilding, evaluation audio from Kyoko at an unseen speed and from the untrained Reed voice passed segmentation and matching at both normal and one-quarter amplitude. The rebuilt model's threshold was approximately 0.551; it was recalibrated from the selected voices rather than changed manually. This does not establish recall or false-positive rates for human speech. More diverse training can also increase false positives, which still require independent negative evaluation.

During development on macOS, a model was generated from nine positive examples using Samantha, Daniel, and Karen, plus six negative examples of "good morning" and "turn off the lights." A small exploratory check with a different voice, Moira, produced the following results. This checks limitations of the current approach; it is not an evaluation of human speech accuracy.

| Phrase | Result |
| --- | --- |
| hey computer | Detected |
| hello computer | False positive |
| hey commuter | False positive |
| open the window | False positive |
| what time is it | Not detected |
| the computer is ready | Not detected |

Relaxing the threshold alone does not adequately distinguish variations in voice from differences between words. Production use requires training and evaluation with more confusable negative examples and a more discriminative detection model. The current template matcher should not be treated as a finished detector.

Further validation is needed for recall on human speech, false positives per hour, noise, reverberation, microphone distance, CPU usage, physical microphones, Linux Docker access, and operation under service managers. Calibration that separates synthetic voices does not replace independent evaluation with human speakers. Moving toward reliable speaker-independent detection will require more diverse TTS data, noise and reverberation augmentation, and an independent human speech evaluation set to compare the current matcher against a small neural model.

Raw TCP JSON Lines, gRPC, SSE, post-detection audio streaming, and pre-roll streaming are out of scope. WebSocket uses TCP for the notification transport described above. If needed later, audio should use a separate socket, with notification protocol extensions considered independently.
