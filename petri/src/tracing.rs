// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use fs_err::File;
use fs_err::PathExt;
use futures::AsyncBufReadExt;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::Stream;
use futures::StreamExt;
use futures::io::BufReader;
use jiff::Timestamp;
use kmsg::KmsgParsedEntry;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Read;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tracing::Level;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// A source of [`PetriLogFile`] log files for test output.
#[derive(Clone, Debug)]
pub struct PetriLogSource(Arc<LogSourceInner>);

#[derive(Debug)]
struct LogSourceInner {
    root_path: PathBuf,
    json_log: JsonLog,
    log_files: Mutex<HashMap<String, PetriLogFile>>,
    attachments: Mutex<HashMap<String, u64>>,
    prefix: Option<String>,
    echo: bool,
}

impl PetriLogSource {
    fn new(root_path: &Path, prefix: Option<String>) -> anyhow::Result<Self> {
        // Canonicalize so that printed attachment paths are most likely to work.
        let root_path = root_path.fs_err_canonicalize()?;
        let jsonl_name = Self::filename_inner(prefix.as_deref(), "petri.jsonl");
        let jsonl = File::create(root_path.join(jsonl_name))?;
        Ok(Self(Arc::new(LogSourceInner {
            json_log: JsonLog(Arc::new(jsonl)),
            root_path,
            log_files: Default::default(),
            attachments: Default::default(),
            prefix,
            echo: true,
        })))
    }
}

impl PetriLogSource {
    /// Create file-only logs without installing a tracing subscriber or writing
    /// attachment markers to stdout. Suitable for standalone command runners.
    pub fn file_only(root_path: &Path) -> anyhow::Result<Self> {
        let root_path = root_path.fs_err_canonicalize()?;
        let jsonl = File::create(root_path.join("petri.jsonl"))?;
        Ok(Self(Arc::new(LogSourceInner {
            root_path,
            json_log: JsonLog(Arc::new(jsonl)),
            log_files: Default::default(),
            attachments: Default::default(),
            prefix: None,
            echo: false,
        })))
    }

    /// Returns a log file for the given name.
    ///
    /// The name should not have an extension; `.log` will be appended
    /// automatically.
    pub fn log_file(&self, name: &str) -> anyhow::Result<PetriLogFile> {
        use std::collections::hash_map::Entry;

        let name = if let Some(prefix) = self.0.prefix.as_ref() {
            format!("{prefix}{name}")
        } else {
            name.to_string()
        };

        let mut log_files = self.0.log_files.lock();
        let log_file = match log_files.entry(name.clone()) {
            Entry::Occupied(occupied_entry) => occupied_entry.get().clone(),
            Entry::Vacant(vacant_entry) => {
                let mut path = self.0.root_path.join(&name);
                // Note that .log is preferred to .txt at least partially
                // because WSL2 and Defender reportedly conspire to make
                // cross-OS .txt file accesses extremely slow.
                path.set_extension("log");
                let file = File::create(&path)?;
                // Write the path to the file in junit attachment syntax to
                // stdout to ensure the file is attached to the test result.
                if self.0.echo {
                    println!("[[ATTACHMENT|{}]]", path.display());
                }
                vacant_entry
                    .insert(PetriLogFile(Arc::new(LogFileInner {
                        file,
                        json_log: self.0.json_log.clone(),
                        source: name.clone(),
                        echo: self.0.echo,
                        subscribers: Default::default(),
                    })))
                    .clone()
            }
        };
        Ok(log_file)
    }

    fn filename_inner(prefix: Option<&str>, name: &str) -> String {
        if let Some(prefix) = prefix {
            format!("{prefix}{name}")
        } else {
            name.to_string()
        }
    }

    fn filename(&self, name: &str) -> String {
        Self::filename_inner(self.0.prefix.as_deref(), name)
    }

    fn attachment_path(&self, name: &str) -> PathBuf {
        let mut attachments = self.0.attachments.lock();

        let next = attachments.entry(name.to_owned()).or_default();
        let name = Path::new(&name);
        let name = if *next == 0 {
            name
        } else {
            let base = name.file_stem().unwrap().to_str().unwrap();
            let extension = name.extension().unwrap_or_default();
            &Path::new(&format!("{}_{}", base, *next)).with_extension(extension)
        };
        *next += 1;
        self.0.root_path.join(name)
    }

    /// Creates a file with the given name and returns a handle to it.
    ///
    /// If the file already exists, a unique name is generated by appending
    /// a number to the base name.
    pub fn create_attachment(&self, filename: &str) -> anyhow::Result<File> {
        self.create_attachment_no_prefix(&self.filename(filename))
    }

    /// Creates a file with the given name and returns a handle to it.
    pub fn create_attachment_no_prefix(&self, filename: &str) -> anyhow::Result<File> {
        let path = self.attachment_path(filename);
        let file = File::create(&path)?;
        self.trace_attachment(&path);
        Ok(file)
    }

    /// Writes the given data to a file with the given name.
    ///
    /// If the file already exists, a unique name is generated by appending
    /// a number to the base name.
    pub fn write_attachment(&self, filename: &str, mut data: impl Read) -> anyhow::Result<PathBuf> {
        let path = self.attachment_path(&self.filename(filename));
        let mut file = File::create(&path)?;
        std::io::copy(&mut data, &mut file)?;
        self.trace_attachment(&path);
        Ok(path)
    }

    /// Copies the given file path to a file with the given name.
    ///
    /// If the file already exists, a unique name is generated by appending
    /// a number to the base name.
    pub fn copy_attachment(
        &self,
        attachment_filename: &str,
        source_path: &Path,
    ) -> anyhow::Result<()> {
        let dest_path = self.attachment_path(&self.filename(attachment_filename));
        fs_err::copy(source_path, &dest_path)?;
        self.trace_attachment(&dest_path);
        Ok(())
    }

    fn trace_attachment(&self, path: &Path) {
        // Just write the relative path to the JSON log.
        self.0
            .json_log
            .write_attachment(path.file_name().unwrap().as_ref());
        if self.0.echo {
            println!("[[ATTACHMENT|{}]]", path.display());
        }
    }

    /// Records that a test with the given name is running in this directory.
    ///
    /// This is written up front, before the test body runs, so that tooling
    /// can identify the test even if the process never gets as far as
    /// [`Self::log_test_result`] -- because it was killed by a timeout, or
    /// crashed hard enough to skip unwinding. A directory with this file but
    /// no result marker is a test that died without reporting.
    pub fn log_test_start(&self, name: &str) {
        fs_err::write(self.0.root_path.join(self.filename("petri.test")), name).unwrap();
    }

    /// Traces and logs the result of a test run in the format expected by our tooling.
    pub fn log_test_result(&self, r: &anyhow::Result<()>, unstable: bool) {
        let (result_path, contents) = match &r {
            Ok(()) => {
                tracing::info!("test passed");
                ("petri.passed", String::new())
            }
            Err(err) if unstable => {
                tracing::warn!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "unstable test failed"
                );
                ("petri.failed_unstable", format!("{err:#}"))
            }
            Err(err) => {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "test failed"
                );
                ("petri.failed", format!("{err:#}"))
            }
        };
        // Write a file to the output directory to indicate whether the test
        // passed, for easy scanning via tools. For a failure the file holds
        // the error, so that tooling can report why without parsing the log.
        fs_err::write(self.0.root_path.join(self.filename(result_path)), contents).unwrap();
    }

    /// Returns the output directory for log files.
    pub fn output_dir(&self) -> &Path {
        &self.0.root_path
    }
}

#[derive(Clone, Debug)]
struct JsonLog(Arc<File>);

impl JsonLog {
    fn write_json(&self, v: &impl serde::Serialize) {
        let v = serde_json::to_vec(v);
        if let Ok(mut v) = v {
            v.push(b'\n');
            // Write once to avoid interleaving JSON entries.
            let _ = self.0.as_ref().write_all(&v);
        }
    }

    fn write_entry(&self, timestamp: Option<Timestamp>, level: Level, source: &str, buf: &[u8]) {
        #[derive(serde::Serialize)]
        struct JsonEntry<'a> {
            timestamp: Timestamp,
            source: &'a str,
            severity: &'a str,
            message: &'a str,
        }
        let message = String::from_utf8_lossy(buf);
        self.write_json(&JsonEntry {
            timestamp: timestamp.unwrap_or_else(Timestamp::now),
            source,
            severity: level.as_str(),
            message: message.trim_ascii(),
        });
    }

    fn write_attachment(&self, path: &Path) {
        #[derive(serde::Serialize)]
        struct JsonEntry<'a> {
            timestamp: Timestamp,
            attachment: &'a Path,
        }
        self.write_json(&JsonEntry {
            timestamp: Timestamp::now(),
            attachment: path,
        });
    }
}

/// Maximum historical lines retained per log file for late subscribers.
const MAX_HISTORY_LINES: usize = 4096;

/// Maximum queued lines per live subscriber before dropping new lines.
const MAX_SUBSCRIBER_QUEUE: usize = 4096;

#[derive(Debug, Default)]
struct LogSubscribers {
    history: VecDeque<String>,
    senders: Vec<async_channel::Sender<String>>,
}

#[derive(Debug)]
struct LogFileInner {
    file: File,
    json_log: JsonLog,
    source: String,
    echo: bool,
    subscribers: Mutex<LogSubscribers>,
}

impl LogFileInner {
    fn write_stdout(&self, buf: &[u8]) {
        if !self.echo {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "[{:>10}] ", self.source).unwrap();
        stdout.write_all(buf).unwrap();
    }
}

struct LogWriter<'a> {
    inner: &'a LogFileInner,
    level: Level,
    timestamp: Option<Timestamp>,
}

impl std::io::Write for LogWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Write to the JSONL file.
        self.inner
            .json_log
            .write_entry(self.timestamp, self.level, &self.inner.source, buf);
        // Write to the specific log file.
        let _ = (&self.inner.file).write_all(buf);
        // Write to stdout, prefixed with the source.
        self.inner.write_stdout(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A stream of entries written to a [`PetriLogFile`].
///
/// Replays buffered entries recorded prior to subscription, followed by live
/// entries as they arrive.
#[derive(Debug)]
pub struct PetriLogStream {
    rx: Pin<Box<async_channel::Receiver<String>>>,
}

impl Stream for PetriLogStream {
    type Item = String;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.as_mut().poll_next(cx)
    }
}

/// A log file for writing test output.
///
/// Generally, you should use [`tracing`] for test-generated logging. This type
/// is for writing fully-formed text entries that come from an external source,
/// such as another process or a guest serial port.
#[derive(Clone, Debug)]
pub struct PetriLogFile(Arc<LogFileInner>);

impl PetriLogFile {
    /// Subscribe to entries written to this log file.
    ///
    /// Replays up to `MAX_HISTORY_LINES` prior entries in order, then streams
    /// subsequent entries as they are written.
    pub fn subscribe(&self) -> PetriLogStream {
        let mut subs = self.0.subscribers.lock();
        let queue_len = subs.history.len().max(MAX_SUBSCRIBER_QUEUE);
        let (tx, rx) = async_channel::bounded(queue_len);
        for line in &subs.history {
            tx.try_send(line.clone())
                .expect("fresh subscriber channel has capacity for history");
        }
        subs.senders.push(tx);
        PetriLogStream { rx: Box::pin(rx) }
    }

    /// Write a log entry with the given format arguments.
    pub fn write_entry_fmt(
        &self,
        timestamp: Option<Timestamp>,
        level: Level,
        args: std::fmt::Arguments<'_>,
    ) {
        let entry = format!("{args}");
        {
            let mut subs = self.0.subscribers.lock();
            subs.senders.retain(|tx| match tx.try_send(entry.clone()) {
                Ok(()) => true,
                Err(err) if err.is_closed() => false,
                Err(err) => {
                    assert!(err.is_full());
                    true
                }
            });
            if subs.history.len() == MAX_HISTORY_LINES {
                subs.history.pop_front();
            }
            subs.history.push_back(entry.clone());
        }
        // Convert to a single string to write to the file to ensure the entry
        // does not get interleaved with other log entries.
        let mut line = entry;
        line.push('\n');
        let _ = LogWriter {
            inner: &self.0,
            level,
            timestamp,
        }
        .write_all(line.as_bytes());
    }

    /// Write a log entry with the given message.
    pub fn write_entry(&self, message: impl std::fmt::Display) {
        self.write_entry_fmt(None, Level::INFO, format_args!("{}", message));
    }
}

/// Write a formatted log entry to the given [`PetriLogFile`].
#[macro_export]
macro_rules! log {
    ($file:expr, $($arg:tt)*) => {
        <$crate::PetriLogFile>::write_entry_fmt(&$file, format_args!($($arg)*))
    };
}

/// Initialize Petri tracing with the given output path for log files.
///
/// Events go to three places:
/// - `petri.jsonl`, in newline-separated JSON format.
/// - standard output, in human readable format.
/// - a log file, in human readable format. This file is `petri.log`, except
///   for events whose target ends in `.log`, which go to separate files named by
///   the target.
pub fn try_init_tracing(
    root_path: &Path,
    default_level: LevelFilter,
    prefix: Option<String>,
) -> anyhow::Result<PetriLogSource> {
    let targets =
        if let Ok(var) = std::env::var("OPENVMM_LOG").or_else(|_| std::env::var("HVLITE_LOG")) {
            var.parse().unwrap()
        } else {
            Targets::new().with_default(default_level)
        };

    // Canonicalize so that printed attachment paths are most likely to work.
    let logger = PetriLogSource::new(root_path, prefix)?;

    let petri_log = logger.log_file("petri")?;

    tracing_subscriber::fmt()
        .compact()
        .with_ansi(false) // avoid polluting logs with escape sequences
        .log_internal_errors(true)
        .with_writer(PetriWriter(petri_log))
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_max_level(LevelFilter::TRACE)
        .finish()
        .with(targets)
        .try_init()?;

    Ok(logger)
}

struct PetriWriter(PetriLogFile);

impl<'a> MakeWriter<'a> for PetriWriter {
    type Writer = LogWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            inner: &self.0.0,
            level: Level::INFO,
            timestamp: None,
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        LogWriter {
            inner: &self.0.0,
            level: *meta.level(),
            timestamp: None,
        }
    }
}

/// Logs lines from `reader` into `log_file`.
///
/// Attempts to parse lines as `SyslogParsedEntry`, extracting the log level.
/// Passes through any non-conforming logs.
pub async fn log_task(
    log_file: PetriLogFile,
    reader: impl AsyncRead + Unpin + Send + 'static,
    name: &str,
) -> anyhow::Result<()> {
    const MAX_LINE_LENGTH: usize = 4096;
    tracing::info!("connected to {name}");
    let mut buf = Vec::with_capacity(MAX_LINE_LENGTH);
    let mut reader = BufReader::new(reader);
    loop {
        buf.clear();
        match (&mut reader)
            .take(MAX_LINE_LENGTH as u64)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) => {
                tracing::info!("disconnected from {name}: EOF");
                return Ok(());
            }
            Err(e) => {
                tracing::info!("disconnected from {name}: error: {e:#}");
                return Err(e.into());
            }
            _ => {}
        }

        let string_buf = String::from_utf8_lossy(&buf);
        let string_buf_trimmed = string_buf.trim_end();

        if let Some(message) = kmsg::SyslogParsedEntry::new(string_buf_trimmed) {
            let level = kernel_level_to_tracing_level(message.level);
            log_file.write_entry_fmt(None, level, format_args!("{}", message.display(false)));
        } else {
            log_file.write_entry(string_buf_trimmed);
        }
    }
}

/// Maps kernel log levels to tracing levels.
fn kernel_level_to_tracing_level(kernel_level: u8) -> Level {
    match kernel_level {
        0..=3 => Level::ERROR,
        4 => Level::WARN,
        5..=6 => Level::INFO,
        7 => Level::DEBUG,
        _ => Level::INFO,
    }
}

/// read from the kmsg stream and write entries to the log
pub async fn kmsg_log_task(
    log_file: PetriLogFile,
    diag_client: diag_client::DiagClient,
) -> anyhow::Result<()> {
    loop {
        diag_client.wait_for_server().await?;
        let mut kmsg = diag_client.kmsg(true).await?;
        tracing::info!("kmsg connected");
        while let Some(data) = kmsg.next().await {
            match data {
                Ok(data) => {
                    let message = KmsgParsedEntry::new(&data).unwrap();
                    let level = kernel_level_to_tracing_level(message.level);
                    log_file.write_entry_fmt(
                        None,
                        level,
                        format_args!("{}", message.display(false)),
                    );
                }
                Err(err) => {
                    tracing::info!("kmsg disconnected: {err:#}");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kernel_level_to_tracing_level() {
        // Test emergency to error levels (0-3)
        assert_eq!(kernel_level_to_tracing_level(0), Level::ERROR);
        assert_eq!(kernel_level_to_tracing_level(1), Level::ERROR);
        assert_eq!(kernel_level_to_tracing_level(2), Level::ERROR);
        assert_eq!(kernel_level_to_tracing_level(3), Level::ERROR);

        // Test warning level (4)
        assert_eq!(kernel_level_to_tracing_level(4), Level::WARN);

        // Test notice and info levels (5-6)
        assert_eq!(kernel_level_to_tracing_level(5), Level::INFO);
        assert_eq!(kernel_level_to_tracing_level(6), Level::INFO);

        // Test debug level (7)
        assert_eq!(kernel_level_to_tracing_level(7), Level::DEBUG);

        // Test unknown level (fallback)
        assert_eq!(kernel_level_to_tracing_level(8), Level::INFO);
        assert_eq!(kernel_level_to_tracing_level(255), Level::INFO);
    }

    #[test]
    fn subscribe_replays_history_and_streams_live_entries() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let source = PetriLogSource::file_only(dir.path()).unwrap();
            let log = source.log_file("guest").unwrap();

            log.write_entry("early-1");
            log.write_entry("early-2");

            let mut stream = log.subscribe();
            log.write_entry("live-3");

            assert_eq!(stream.next().await.as_deref(), Some("early-1"));
            assert_eq!(stream.next().await.as_deref(), Some("early-2"));
            assert_eq!(stream.next().await.as_deref(), Some("live-3"));
        });
    }

    #[test]
    fn subscribe_evicts_oldest_history_and_prunes_dropped_subscribers() {
        futures::executor::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let source = PetriLogSource::file_only(dir.path()).unwrap();
            let log = source.log_file("guest").unwrap();

            let dropped_stream = log.subscribe();
            drop(dropped_stream);

            for i in 0..(MAX_HISTORY_LINES + 5) {
                log.write_entry(format_args!("line-{i}"));
            }
            assert_eq!(log.0.subscribers.lock().senders.len(), 0);

            let mut stream = log.subscribe();
            assert_eq!(stream.next().await.as_deref(), Some("line-5"));
        });
    }
}
