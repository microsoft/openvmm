// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use diag_client::kmsg_stream::KmsgStream;
use fs_err::File;
use fs_err::PathExt;
use futures::io::BufReader;
use futures::AsyncBufReadExt;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::StreamExt;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::level_filters::LevelFilter;
use tracing::Level;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// A source of [`PetriLogFile`] log files for test output.
#[derive(Clone)]
pub struct PetriLogSource(Arc<LogSourceInner>);

struct LogSourceInner {
    root_path: PathBuf,
    json_log: JsonLog,
    log_files: Mutex<HashMap<String, PetriLogFile>>,
    attachments: Mutex<HashMap<String, u64>>,
}

impl PetriLogSource {
    /// Returns a log file for the given name.
    ///
    /// The name should not have an extension; `.log` will be appended
    /// automatically.
    pub fn log_file(&self, name: &str) -> anyhow::Result<PetriLogFile> {
        use std::collections::hash_map::Entry;

        let mut log_files = self.0.log_files.lock();
        let log_file = match log_files.entry(name.to_owned()) {
            Entry::Occupied(occupied_entry) => occupied_entry.get().clone(),
            Entry::Vacant(vacant_entry) => {
                let mut path = self.0.root_path.join(name);
                // Note that .log is preferred to .txt at least partially
                // because WSL2 and Defender reportedly conspire to make
                // cross-OS .txt file accesses extremely slow.
                path.set_extension("log");
                let file = File::create(&path)?;
                // Write the path to the file in junit attachment syntax to
                // stdout to ensure the file is attached to the test result.
                println!("[[ATTACHMENT|{}]]", path.display());
                vacant_entry
                    .insert(PetriLogFile(Arc::new(LogFileInner {
                        file,
                        json_log: self.0.json_log.clone(),
                        source: name.to_owned(),
                    })))
                    .clone()
            }
        };
        Ok(log_file)
    }

    fn attachment_path(&self, name: &str) -> PathBuf {
        let mut attachments = self.0.attachments.lock();
        let next = attachments.entry(name.to_owned()).or_default();
        let name = Path::new(name);
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
        let path = self.attachment_path(filename);
        let file = File::create(&path)?;
        self.trace_attachment(&path);
        Ok(file)
    }

    /// Writes the given data to a file with the given name.
    ///
    /// If the file already exists, a unique name is generated by appending
    /// a number to the base name.
    pub fn write_attachment(
        &self,
        filename: &str,
        data: impl AsRef<[u8]>,
    ) -> anyhow::Result<PathBuf> {
        let path = self.attachment_path(filename);
        fs_err::write(&path, data)?;
        self.trace_attachment(&path);
        Ok(path)
    }

    fn trace_attachment(&self, path: &Path) {
        // Just write the relative path to the JSON log.
        self.0
            .json_log
            .write_attachment(path.file_name().unwrap().as_ref());
        println!("[[ATTACHMENT|{}]]", path.display());
    }
}

#[derive(Clone)]
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

    fn write_entry(&self, level: Level, source: &str, buf: &[u8]) {
        #[derive(serde::Serialize)]
        struct JsonEntry<'a> {
            timestamp: jiff::Timestamp,
            source: &'a str,
            severity: &'a str,
            message: &'a str,
        }
        let message = String::from_utf8_lossy(buf);
        self.write_json(&JsonEntry {
            timestamp: jiff::Timestamp::now(),
            source,
            severity: level.as_str(),
            message: message.trim_ascii(),
        });
    }

    fn write_attachment(&self, path: &Path) {
        #[derive(serde::Serialize)]
        struct JsonEntry<'a> {
            timestamp: jiff::Timestamp,
            attachment: &'a Path,
        }
        self.write_json(&JsonEntry {
            timestamp: jiff::Timestamp::now(),
            attachment: path,
        });
    }
}

struct LogFileInner {
    file: File,
    json_log: JsonLog,
    source: String,
}

impl LogFileInner {
    fn write_stdout(&self, buf: &[u8]) {
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "[{:>10}] ", self.source).unwrap();
        stdout.write_all(buf).unwrap();
    }
}

struct LogWriter<'a> {
    inner: &'a LogFileInner,
    level: Level,
}

impl std::io::Write for LogWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Write to the JSONL file.
        self.inner
            .json_log
            .write_entry(self.level, &self.inner.source, buf);
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

/// A log file for writing test output.
///
/// Generally, you should use [`tracing`] for test-generated logging. This type
/// is for writing fully-formed text entries that come from an external source,
/// such as another process or a guest serial port.
#[derive(Clone)]
pub struct PetriLogFile(Arc<LogFileInner>);

impl PetriLogFile {
    /// Write a log entry with the given format arguments.
    pub fn write_entry_fmt(&self, args: std::fmt::Arguments<'_>) {
        // Convert to a single string to write to the file to ensure the entry
        // does not get interleaved with other log entries.
        let _ = LogWriter {
            inner: &self.0,
            level: Level::INFO,
        }
        .write_all(format!("{}\n", args).as_bytes());
    }

    /// Write a log entry with the given message.
    pub fn write_entry(&self, message: impl std::fmt::Display) {
        self.write_entry_fmt(format_args!("{}", message));
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
pub fn try_init_tracing(root_path: &Path) -> anyhow::Result<PetriLogSource> {
    let targets =
        if let Ok(var) = std::env::var("OPENVMM_LOG").or_else(|_| std::env::var("HVLITE_LOG")) {
            var.parse().unwrap()
        } else {
            Targets::new().with_default(LevelFilter::DEBUG)
        };

    // Canonicalize so that printed attachment paths are most likely to work.
    let root_path = root_path.fs_err_canonicalize()?;
    let jsonl = File::create(root_path.join("petri.jsonl"))?;
    let logger = PetriLogSource(Arc::new(LogSourceInner {
        json_log: JsonLog(Arc::new(jsonl)),
        root_path,
        log_files: Default::default(),
        attachments: Default::default(),
    }));

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
            inner: &self.0 .0,
            level: Level::INFO,
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        LogWriter {
            inner: &self.0 .0,
            level: *meta.level(),
        }
    }
}

/// read from the serial reader and write entries to the log
pub async fn serial_log_task(
    log_file: PetriLogFile,
    reader: impl AsyncRead + Unpin + Send + 'static,
) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    let mut reader = BufReader::new(reader);
    loop {
        buf.clear();
        let n = (&mut reader).take(256).read_until(b'\n', &mut buf).await?;
        if n == 0 {
            break;
        }

        let string_buf = String::from_utf8_lossy(&buf);
        let string_buf_trimmed = string_buf.trim_end();
        log_file.write_entry(string_buf_trimmed);
    }
    Ok(())
}

/// read from the kmsg stream and write entries to the log
pub async fn kmsg_log_task(
    log_file: PetriLogFile,
    mut file_stream: KmsgStream,
) -> anyhow::Result<()> {
    while let Some(data) = file_stream.next().await {
        match data {
            Ok(data) => {
                let message = kmsg::KmsgParsedEntry::new(&data)?;
                log_file.write_entry(message.display(false));
            }
            Err(err) => {
                tracing::info!("kmsg disconnected: {err:?}");
                break;
            }
        }
    }

    Ok(())
}
