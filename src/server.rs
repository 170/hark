use crate::feedback::Label;
use crate::registry::{Registry, validate_word};
use crate::transport::{self, MAX_MESSAGE, Reader, Writer};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub event_id: String,
    pub word: String,
    pub ts: f64,
    /// Similarity score, not a calibrated probability.
    pub score: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    subscribe: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackRequest {
    feedback: Option<FeedbackLabel>,
    reset: Option<ResetRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetRequest {
    word: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackLabel {
    event_id: String,
    label: Label,
}

fn parse_feedback(line: &[u8]) -> Result<FeedbackRequest> {
    anyhow::ensure!(
        line.len() <= MAX_MESSAGE,
        "feedback must be a JSON message of at most 4096 bytes"
    );
    let request: FeedbackRequest = serde_json::from_slice(line)?;
    anyhow::ensure!(
        request.feedback.is_some() != request.reset.is_some(),
        "send exactly one feedback or reset request"
    );
    if let Some(feedback) = &request.feedback {
        anyhow::ensure!(
            !feedback.event_id.is_empty() && feedback.event_id.len() <= 128,
            "event_id must contain 1 to 128 bytes"
        );
    }
    if let Some(reset) = &request.reset {
        validate_word(&reset.word)?;
    }
    Ok(request)
}

struct Subscriber {
    id: u64,
    words: HashSet<String>,
    tx: mpsc::Sender<Event>,
    cancel: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Hub(Arc<Mutex<(u64, Vec<Subscriber>)>>, Arc<Semaphore>);

impl Default for Hub {
    fn default() -> Self {
        Self(
            Arc::new(Mutex::new((0, Vec::new()))),
            Arc::new(Semaphore::new(128)),
        )
    }
}

impl Hub {
    pub(crate) fn reserve_client(&self) -> Option<OwnedSemaphorePermit> {
        self.1.clone().try_acquire_owned().ok()
    }
    /// No socket I/O or waiting for channel capacity in the detector's path.
    pub fn publish(&self, event: Event) {
        let mut state = self.0.lock().unwrap();
        state.1.retain(|sub| {
            if !sub.words.contains(&event.word) {
                return true;
            }
            if sub.tx.try_send(event.clone()).is_err() {
                let _ = sub.cancel.send(true);
                false
            } else {
                true
            }
        });
    }

    fn add(
        &self,
        words: HashSet<String>,
        capacity: usize,
    ) -> (u64, mpsc::Receiver<Event>, watch::Receiver<bool>) {
        let (tx, rx) = mpsc::channel(capacity);
        let (cancel, cancelled) = watch::channel(false);
        let mut state = self.0.lock().unwrap();
        state.0 += 1;
        let id = state.0;
        state.1.push(Subscriber {
            id,
            words,
            tx,
            cancel,
        });
        (id, rx, cancelled)
    }

    fn remove(&self, id: u64) {
        self.0.lock().unwrap().1.retain(|sub| sub.id != id);
    }
    pub fn subscriber_count(&self) -> usize {
        self.0.lock().unwrap().1.len()
    }
}

struct Registration(Hub, u64);
impl Drop for Registration {
    fn drop(&mut self) {
        self.0.remove(self.1);
    }
}

/// Hold a process lock for the socket's lifetime. Never unlink the lock file:
/// unlinking it would let two processes lock different inodes.
pub struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
    _lock: File,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn bind(path: &Path) -> Result<(UnixListener, SocketGuard)> {
    let parent = path
        .parent()
        .context("socket must have a parent directory")?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o750))?;
    }
    let meta = fs::symlink_metadata(parent)?;
    // The directory must not be writable by subscribers: they could replace the socket.
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        bail!("socket directory must be owned by this user and not group/world writable");
    }
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(lock_path)?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another hark instance holds the socket lock");
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => bail!("socket is already in use"),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(path)?
                }
                Err(e) => return Err(e).context("cannot verify stale socket"),
            }
        }
        Ok(_) => bail!("refusing to remove a non-socket path"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)?;
    let meta = fs::symlink_metadata(path)?;
    let guard = SocketGuard {
        path: path.into(),
        dev: meta.dev(),
        ino: meta.ino(),
        _lock: lock,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o660))?;
    Ok((listener, guard))
}

pub fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    #[cfg(target_os = "linux")]
    let (mut cred, level, option) = (
        unsafe { std::mem::zeroed::<libc::ucred>() },
        libc::SOL_SOCKET,
        libc::SO_PEERCRED,
    );
    #[cfg(target_os = "macos")]
    let (mut cred, level, option) = (
        unsafe { std::mem::zeroed::<libc::xucred>() },
        libc::SOL_LOCAL,
        libc::LOCAL_PEERCRED,
    );
    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
    // SAFETY: cred is an initialized platform credential struct; len describes its writable size.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            level,
            option,
            std::ptr::from_mut(&mut cred).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of_val(&cred) {
        return Err(std::io::Error::other("invalid peer credential size"));
    }
    #[cfg(target_os = "macos")]
    {
        if cred.cr_version != libc::XUCRED_VERSION {
            return Err(std::io::Error::other("invalid peer credential version"));
        }
        Ok(cred.cr_uid)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(cred.uid)
    }
}

fn parse_subscription(line: &[u8]) -> Result<HashSet<String>> {
    if line.len() > MAX_MESSAGE {
        bail!("subscription must be a JSON message of at most 4096 bytes");
    }
    let subscription: Subscription = serde_json::from_slice(line)?;
    if subscription.subscribe.is_empty() || subscription.subscribe.len() > 64 {
        bail!("subscribe must contain 1 to 64 words");
    }
    let words: HashSet<_> = subscription.subscribe.into_iter().collect();
    for word in &words {
        validate_word(word)?;
    }
    Ok(words)
}

async fn client(stream: UnixStream, hub: Hub, models: Registry, capacity: usize) -> Result<()> {
    let peer = format!("uid={}", peer_uid(&stream)?);
    let (read, write) = transport::unix(stream);
    run_session(read, write, hub, models, capacity, peer).await
}

pub(crate) async fn run_session(
    read: Reader,
    mut write: Writer,
    hub: Hub,
    models: Registry,
    capacity: usize,
    peer: String,
) -> Result<()> {
    eprintln!("client connected {peer}");
    let result = session(read, &mut write, hub, models, capacity, &peer).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), write.close()).await;
    eprintln!("client disconnected {peer}");
    result
}

async fn session(
    mut reader: Reader,
    write: &mut Writer,
    hub: Hub,
    models: Registry,
    capacity: usize,
    peer: &str,
) -> Result<()> {
    let result = tokio::time::timeout(Duration::from_secs(5), reader.read()).await;
    let parsed = match result {
        Ok(Ok(Some(line))) => parse_subscription(&line),
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow::anyhow!("subscription timeout")),
    };
    let words = match parsed {
        Ok(words) => words,
        Err(e) => {
            let error =
                serde_json::json!({"error": "invalid_subscription", "message": e.to_string()});
            let _ = tokio::time::timeout(Duration::from_secs(1), write.send(&error)).await;
            return Ok(());
        }
    };
    let prepared = tokio::select! {
        result = models.ensure(&words) => result,
        _ = reader.read() => return Ok(()),
    };
    if let Err(error) = prepared {
        let message =
            serde_json::json!({"error": "model_unavailable", "message": error.to_string()});
        let _ = tokio::time::timeout(Duration::from_secs(1), write.send(&message)).await;
        return Ok(());
    }
    eprintln!("subscription ready {peer} words={words:?}");
    let mut ready_words: Vec<_> = words.iter().cloned().collect();
    ready_words.sort();
    let ready = serde_json::json!({"event": "ready", "words": ready_words});
    let (id, mut events, mut cancelled) = hub.add(words.clone(), capacity);
    let _registration = Registration(hub, id);
    let (replies, mut responses) =
        mpsc::channel::<(serde_json::Value, Option<oneshot::Sender<()>>)>(capacity);
    let send = async {
        // Register first so detections can queue, but always write readiness
        // before draining events. The same timeout/cancellation covers both.
        tokio::time::timeout(Duration::from_secs(5), write.send(&ready)).await??;
        loop {
            let (response, written) = tokio::select! {
                Some(event) = events.recv() => (serde_json::to_value(event)?, None),
                Some(response) = responses.recv() => response,
                else => break,
            };
            tokio::time::timeout(Duration::from_secs(5), write.send(&response)).await??;
            if let Some(written) = written {
                let _ = written.send(());
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let receive = async {
        loop {
            let parsed = match reader.read().await {
                Ok(Some(line)) => parse_feedback(&line),
                Ok(None) => break,
                Err(error) => Err(error),
            };
            let request = match parsed {
                Ok(request) => request,
                Err(error) => {
                    let (written, done) = oneshot::channel();
                    replies.send((serde_json::json!({"error": "invalid_feedback", "message": error.to_string()}), Some(written))).await?;
                    // Let the writer send the error before closing the connection.
                    let _ = done.await;
                    return Ok::<_, anyhow::Error>(());
                }
            };
            let feedback = models.feedback.clone();
            let allowed = words.clone();
            let response = if let Some(report) = request.feedback {
                let event_id = report.event_id;
                let reported_id = event_id.clone();
                let label = report.label;
                let result = tokio::task::spawn_blocking(move || {
                    feedback.apply_label(&reported_id, label, &allowed)
                })
                .await?;
                match result {
                    Ok(reply) => {
                        eprintln!(
                            "feedback {peer} event_id={event_id} label={label:?} status={}",
                            reply.status
                        );
                        serde_json::to_value(reply)?
                    }
                    Err(error) => {
                        serde_json::json!({"error": "feedback_rejected", "event_id": event_id, "message": error.to_string()})
                    }
                }
            } else {
                let word = request.reset.unwrap().word;
                let reset_word = word.clone();
                let result =
                    tokio::task::spawn_blocking(move || feedback.reset(&reset_word, &allowed))
                        .await?;
                match result {
                    Ok(reply) => {
                        eprintln!("feedback reset {peer} word={word:?}");
                        serde_json::to_value(reply)?
                    }
                    Err(error) => {
                        serde_json::json!({"error": "reset_rejected", "word": word, "message": error.to_string()})
                    }
                }
            };
            replies.send((response, None)).await?;
        }
        Ok(())
    };
    tokio::select! {
        result = send => { result?; },
        _ = cancelled.changed() => {},
        result = receive => { result?; },
    }
    Ok(())
}

pub async fn serve(
    listener: UnixListener,
    hub: Hub,
    models: Registry,
    capacity: usize,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    anyhow::ensure!(capacity > 0, "queue capacity must be positive");
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            result = clients.join_next(), if !clients.is_empty() => {
                if let Some(Err(e)) = result { eprintln!("client task failed: {e}"); }
            },
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Some(permit) = hub.reserve_client() else { drop(stream); continue; };
                let (hub, models) = (hub.clone(), models.clone());
                clients.spawn(async move {
                    let _permit = permit;
                    if let Err(e) = client(stream, hub, models, capacity).await { eprintln!("client error: {e:#}"); }
                });
            }
        }
    }
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn feedback_and_reset_are_mutually_exclusive_and_strict() {
        assert!(
            parse_feedback(br#"{"feedback":{"event_id":"id","label":"true_positive"}}"#).is_ok()
        );
        assert!(parse_feedback(br#"{"reset":{"word":"hello"}}"#).is_ok());
        for invalid in [
            r#"{}"#,
            r#"{"feedback":{"event_id":"id","label":"true_positive"},"reset":{"word":"hello"}}"#,
            r#"{"reset":{"word":"../hello"}}"#,
            r#"{"reset":{"word":"hello","all":true}}"#,
            r#"{"reset":{"word":"hello"},"extra":true}"#,
        ] {
            assert!(parse_feedback(invalid.as_bytes()).is_err(), "{invalid}");
        }
    }

    #[test]
    fn connection_limit_is_shared_and_released_across_hub_clones() {
        let hub = Hub::default();
        let other = hub.clone();
        let mut permits: Vec<_> = (0..128).map(|_| hub.reserve_client().unwrap()).collect();
        assert!(other.reserve_client().is_none());
        permits.pop();
        assert!(other.reserve_client().is_some());
    }

    #[test]
    fn strict_subscription() {
        assert!(parse_subscription(b"{\"subscribe\":[\"hey-computer\"]}\n").is_ok());
        assert!(parse_subscription(b"{\"subscribe\":[\"new-word\"]}\n").is_ok());
        for bad in [
            "{}\n",
            "{\"subscribe\":[]}\n",
            "{\"subscribe\":[\"../unknown\"]}\n",
            "{\"subscribe\":[\"   \"]}\n",
            "{\"subscribe\":[\"hey-computer\"],\"x\":1}\n",
            "{\"subscribe\":[\"hey-computer\"]} {}",
        ] {
            assert!(parse_subscription(bad.as_bytes()).is_err());
        }
    }
    #[test]
    fn slow_subscriber_does_not_block_others() {
        let hub = Hub::default();
        let words = HashSet::from(["hey-computer".to_string()]);
        let (_, _slow, cancel) = hub.add(words.clone(), 1);
        let (_, mut fast, _) = hub.add(words, 1);
        let event = Event {
            event: "detected".into(),
            event_id: "test-event".into(),
            word: "hey-computer".into(),
            ts: 1.0,
            score: 0.9,
        };
        hub.publish(event.clone());
        assert!(fast.try_recv().is_ok());
        hub.publish(event);
        assert!(*cancel.borrow());
        assert!(fast.try_recv().is_ok());
        assert_eq!(hub.subscriber_count(), 1);
    }
}
