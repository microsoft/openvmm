// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements sending crash dump files to the host.

#![cfg(target_os = "linux")]
#![expect(missing_docs)]
// UNSAFETY: Calling libc functions to gather system information, and manipulating
// stdout & stderr.
#![expect(unsafe_code)]

mod elf;
mod options;
mod proto;

// `pub` so that the missing_docs warning fires for options without
// documentation.
pub use options::Options;

use crate::elf::Elf64_Ehdr;
use crate::elf::Elf64_Nhdr;
use crate::elf::Elf64_Phdr;
use crate::elf::PT_NOTE;
use crate::proto::check_header;
use crate::proto::make_header;
use anyhow::Context;
use fs_err::File;
use fs_err::os::unix::fs::OpenOptionsExt;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::FutureExt;
use futures::io::AllowStdIo;
use get_protocol::crash;
use get_protocol::crash::Header;
use libc::O_NONBLOCK;
use libc::STDERR_FILENO;
use libc::STDOUT_FILENO;
use pal_async::local::block_with_io;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::pin::pin;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_async::pipe::MessageReadHalf;
use vmbus_async::pipe::MessageWriteHalf;
use vmbus_ring::RingMem;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const KMSG_NOTE_BYTES: usize = 1024 * 256; // 256 KB

struct OsVersionInfo {
    banner: [u8; 256],
    major_minor: (u32, u32),
}

impl OsVersionInfo {
    pub fn new() -> Self {
        let mut banner = [0u8; 256];
        if let Ok(version) = std::fs::read("/proc/version") {
            // NULL-terminate for the case anything expect such string.
            let bytes_to_copy = std::cmp::min(banner.len() - 1, version.len());
            banner[..bytes_to_copy].copy_from_slice(&version[..bytes_to_copy]);
        }

        let major_minor = {
            // SAFETY: zero is a valid bit pattern for the members of the structure
            let mut utsname: libc::utsname = unsafe { std::mem::zeroed() };
            // SAFETY: calling the function according to the documentation
            if unsafe { libc::uname(&mut utsname) } == 0 {
                // SAFETY: the OS uses ASCII characters which form a valid UTF-8 string
                let release = unsafe { std::str::from_utf8_unchecked(utsname.release.as_bytes()) };
                let mut parts = release.split('.').take(2);
                let major_iter = parts.next();
                let minor_iter = parts.next();
                match (major_iter, minor_iter) {
                    (Some(major), None) => (major.parse().unwrap_or(0), 0),
                    (Some(major), Some(minor)) => {
                        (major.parse().unwrap_or(0), minor.parse().unwrap_or(0))
                    }
                    _ => (0, 0),
                }
            } else {
                (0, 0)
            }
        };

        Self {
            banner,
            major_minor,
        }
    }

    pub fn major(&self) -> u32 {
        self.major_minor.0
    }

    pub fn minor(&self) -> u32 {
        self.major_minor.1
    }

    pub fn banner(&self) -> &[u8; 256] {
        &self.banner
    }
}

async fn read_message<T: IntoBytes + FromBytes + Immutable + KnownLayout, M: RingMem>(
    pipe: &mut MessageReadHalf<'_, M>,
) -> anyhow::Result<T> {
    let mut message = T::new_zeroed();
    pipe.recv_exact(message.as_mut_bytes()).await?;
    let header = Header::read_from_prefix(message.as_bytes()).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    check_header(&header)?;
    Ok(message)
}

async fn send_dump<M: RingMem>(
    mut pipe: MessagePipe<M>,
    dump_stream: &mut (impl AsyncRead + Unpin),
    os_version: &OsVersionInfo,
    include_kmsg: bool,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = pipe.split();

    // Negotiate version and capabilities

    let cap_rq = crash::DumpCapabilitiesRequestV1 {
        header: make_header(None, crash::MessageType::REQUEST_GET_CAPABILITIES_V1),
    };
    writer.send(cap_rq.as_bytes()).await?;
    let cap_resp: crash::DumpCapabilitiesResponseV1 = read_message(&mut reader).await?;
    let caps = cap_resp.capabilities;

    if !caps.linux_config_v1() {
        anyhow::bail!("Nix dump files are not supported by the host");
    }

    let max_dump_size = {
        let cfg_rq = crash::DumpConfigRequestV1 {
            header: make_header(None, crash::MessageType::REQUEST_GET_NIX_DUMP_CONFIG_V1),
        };
        writer.send(cfg_rq.as_bytes()).await?;
        let cfg_resp: crash::DumpConfigResponseV1 = read_message(&mut reader).await?;
        let cfg = cfg_resp.config;

        if cfg.max_dump_size == 0 {
            anyhow::bail!("The host does not allow sending crash dump files");
        }
        let dump_type = cfg.dump_type;
        if dump_type != crash::DumpType::ELF {
            anyhow::bail!("The host does not accept ELF core dump files");
        }

        cfg.max_dump_size
    };

    tracing::debug!(max_dump_size, "Got host config");

    let dump_start_rq = crash::DumpStartRequestV1 {
        header: make_header(None, crash::MessageType::REQUEST_NIX_DUMP_START_V1),
    };
    writer.send(dump_start_rq.as_bytes()).await?;
    let dump_start_resp: crash::DumpStartResponseV1 = read_message(&mut reader).await?;

    let start_status = dump_start_resp.status;
    if start_status != 0 {
        anyhow::bail!("The host reported error 0x{:x}", start_status);
    }

    // The VSP may occasionally send an error code, so running
    // reads and writes in parallel. If the VSP signals an error,
    // here the read_task future will exit with an error, and this
    // function will error out. Write and channel failures also fail
    // the transfer.

    let write_task = pin!(async move {
        let mut buf = [0u8; crash::MAX_DUMP_PAYLOAD_SIZE];
        let now = std::time::Instant::now();

        let mut streamer = DumpStreamer::new(
            writer,
            dump_stream,
            dump_start_resp.header,
            max_dump_size as usize,
        );

        if include_kmsg {
            if let Err(e) = streamer.insert_kmsg_note(&mut buf).await {
                tracing::error!("Error occurred while adding kmsg note: {:?}", e);
            }
        }

        streamer
            .stream_all(&mut buf)
            .await
            .context("failed to stream dump")?;

        streamer
            .complete(os_version)
            .await
            .context("failed to complete dump")?;

        // Compute stats
        let wrote_bytes_total = streamer.wrote_bytes_total();
        let nanos = now.elapsed().as_nanos();
        let speed = (wrote_bytes_total as u128 * 1_000_000_000)
            .checked_div(nanos)
            .unwrap_or(0);
        tracing::info!(size = wrote_bytes_total, speed, "Reported crash");

        Ok::<(), anyhow::Error>(())
    });

    let read_task = pin!(wait_for_host_error(&mut reader));

    // Host-reported errors take precedence over a write task that finishes
    // in the same poll.
    futures::select_biased! {
        result = read_task.fuse() => result,
        result = write_task.fuse() => result,
    }
}

/// Waits for the host to report a dump write error.
///
/// The host only responds to dump writes on failure, so this returns only
/// on a host error or a channel failure.
async fn wait_for_host_error<M: RingMem>(
    reader: &mut MessageReadHalf<'_, M>,
) -> anyhow::Result<()> {
    loop {
        let dump_write_resp: crash::DumpWriteResponseV1 = read_message(reader)
            .await
            .context("failed to receive dump write response")?;
        let resp_status = dump_write_resp.status;
        if resp_status != 0 {
            anyhow::bail!("Host error {resp_status:#x}");
        }
    }
}

// This is resilient against recursive crashing as long as
// RLIMIT_CORE is set to 1 by underhill_init.
// To test:
//      1. PS D:\> ohcldiag-dev shell <the-hcl-vm>
//      2. # sleep 10000
//      3. Ctrl+\
// The `main` function returns the "never" type as it does not
// exit back into the standard library. Instead, it tells the OS
// to terminate the process in hopes to be more performant and resilient.
pub fn main() -> ! {
    // Parse options before redirecting stderr and stdout so usage can get printed.
    let options = Options::parse();

    // Now set stderr and stdout to /dev/ttyprintk to catch any other output.
    if !options.no_redirect {
        let ttyprintk = OpenOptions::new().write(true).open("/dev/ttyprintk");
        if let Ok(ttyprintk) = &ttyprintk {
            // SAFETY: calling as documented.
            unsafe {
                libc::dup2(ttyprintk.as_raw_fd(), STDOUT_FILENO);
                libc::dup2(ttyprintk.as_raw_fd(), STDERR_FILENO);
            }
        }
    }

    // Set up logging
    tracing_subscriber::fmt()
        .with_max_level(if options.verbose {
            tracing::Level::TRACE
        } else {
            tracing::Level::INFO
        })
        .log_internal_errors(true)
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .compact()
        .with_ansi(false)
        .init();

    // We should have checks in our callers so this is never hit, but let's be safe.
    if underhill_confidentiality::confidential_filtering_enabled() {
        tracing::info!("crash reporting disabled due to CVM");
        std::process::exit(libc::EXIT_FAILURE);
    }

    let os_version = OsVersionInfo::new();

    let crate_revision = option_env!("BUILD_GIT_SHA").unwrap_or("UNKNOWN_REVISION");
    let openhcl_version = option_env!("OPENHCL_VERSION").unwrap_or("UNKNOWN_VERSION");

    let os_version_major = os_version.major();
    let os_version_minor = os_version.minor();
    tracing::error!(
        ?crate_revision,
        ?openhcl_version,
        ?options.comm,
        ?options.pid,
        ?options.tid,
        ?options.sig,
        ?os_version_major,
        ?os_version_minor,
        ?options.timeout,
        "Process crashed"
    );

    // The watchdog thread

    let _watchdog = std::thread::spawn(move || {
        std::thread::sleep(options.timeout);
        tracing::error!("Crash reporting timed out");
        std::process::exit(-libc::ETIMEDOUT);
    });

    // Send the dump file

    if let Err(e) = block_with_io(async |driver| {
        let mut dump_stream = AllowStdIo::new(std::io::stdin());
        let pipe = vmbus_user_channel::message_pipe(
            &driver,
            vmbus_user_channel::open_uio_device(&crash::CRASHDUMP_GUID)?,
        )?;
        send_dump(pipe, &mut dump_stream, &os_version, !options.no_kmsg).await?;

        Ok::<(), anyhow::Error>(())
    }) {
        tracing::error!(?e, "crash dump error");
        std::process::exit(-libc::EXIT_FAILURE)
    }

    std::process::exit(libc::EXIT_SUCCESS)
}

/// provides useful functions for streaming a core dump
/// and maintains state
struct DumpStreamer<'a, M: RingMem> {
    dump_stream: &'a mut (dyn AsyncRead + Unpin),
    writer: MessageWriteHalf<'a, M>,

    header: Header,
    max_dump_size: usize,

    read_bytes_total: usize,
    wrote_bytes_total: usize,
}

impl<'a, M: RingMem> DumpStreamer<'a, M> {
    fn new(
        writer: MessageWriteHalf<'a, M>,
        dump_stream: &'a mut (impl AsyncRead + Unpin),
        header: Header,
        max_dump_size: usize,
    ) -> Self {
        Self {
            dump_stream,
            writer,
            header,
            max_dump_size,
            read_bytes_total: 0,
            wrote_bytes_total: 0,
        }
    }

    /// read the incoming dump, optionally until the buffer is full
    async fn read(&mut self, buf: &mut [u8], fill: bool) -> usize {
        let mut n = 0;
        while let Ok(read_bytes) = self.dump_stream.read(&mut buf[n..]).await {
            n += read_bytes;
            if !fill || read_bytes == 0 || n >= buf.len() {
                break;
            }
        }
        self.read_bytes_total += n;
        if fill && n != buf.len() {
            tracing::error!(
                "Unable to fill buffer. Expected {:#x}, got {:#x}",
                buf.len(),
                n
            );
        }
        n
    }

    /// write data to the host
    async fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        if self.wrote_bytes_total < self.max_dump_size {
            let can_write_bytes = if self.wrote_bytes_total + data.len() > self.max_dump_size {
                tracing::error!("Dump has been partially sent due to the dump size limit");
                self.max_dump_size - self.wrote_bytes_total
            } else {
                data.len()
            };

            let mut data_next = Some(&data[..can_write_bytes]);

            while let Some(data) = data_next {
                let data = if data.len() <= crash::MAX_DUMP_PAYLOAD_SIZE {
                    data_next = None;
                    data
                } else {
                    data_next = Some(&data[crash::MAX_DUMP_PAYLOAD_SIZE..]);
                    &data[..crash::MAX_DUMP_PAYLOAD_SIZE]
                };

                // Send the write request to announce the data packet
                let dump_write_rq = crash::DumpWriteRequestV1 {
                    header: make_header(
                        Some(&self.header),
                        crash::MessageType::REQUEST_NIX_DUMP_WRITE_V1,
                    ),
                    offset: self.wrote_bytes_total as u64,
                    size: data.len() as u32,
                };
                self.writer.send(dump_write_rq.as_bytes()).await?;

                // Send the dump data
                self.writer.send(data).await?;

                self.wrote_bytes_total += data.len();
            }
        }
        Ok(())
    }

    /// stream the rest of the dump to the host
    async fn stream_all(&mut self, buf: &mut [u8]) -> anyhow::Result<()> {
        loop {
            let n = self.read(buf, false).await;
            if n == 0 {
                break;
            }
            self.write(&buf[..n]).await?;
        }
        Ok(())
    }

    /// stream a specific number of bytes of the dump to the host
    async fn stream_n(&mut self, buf: &mut [u8], bytes: usize) -> anyhow::Result<()> {
        let mut total = 0;
        while total < bytes {
            let remaining = if total + buf.len() > bytes {
                bytes - total
            } else {
                buf.len()
            };

            let n = self.read(&mut buf[..remaining], false).await;
            if n == 0 {
                break;
            }
            self.write(&buf[..n]).await?;
            total += n;
        }
        if bytes != total {
            tracing::error!("Unable to stream {:#x} bytes, got {:#x}", bytes, total);
        }
        Ok(())
    }

    /// stream a non-blocking file to the host
    async fn stream_file(&mut self, buf: &mut [u8], file: &mut File, max_len: usize) -> usize {
        let mut total = 0;
        loop {
            match file.read(buf) {
                // if eof or would block, we are done
                Ok(0) => break,
                Err(ref err) if err.kind() == ErrorKind::WouldBlock => break,
                // continue on interruptions or broken pipe, since
                // if old messages are overwritten while /dev/kmsg is open,
                // the next read returns -EPIPE
                Err(ref err)
                    if err.kind() == ErrorKind::Interrupted
                        || err.kind() == ErrorKind::BrokenPipe => {}
                // append the data
                Ok(len) => {
                    if total + len > max_len {
                        tracing::error!("file will be truncated.");
                        let len = max_len - total;
                        total += len;
                        if let Err(e) = self.write(&buf[..len]).await {
                            tracing::error!("error writing file: {:?}", e);
                        }
                        break;
                    }
                    total += len;
                    if let Err(e) = self.write(&buf[..len]).await {
                        tracing::error!("error writing file: {:?}", e);
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("error reading file: {:?}", e);
                    break;
                }
            }
        }
        total
    }

    /// write bytes of padding to the host use buf for scratch
    async fn write_padding(&mut self, buf: &mut [u8], bytes: usize) -> anyhow::Result<()> {
        buf.fill(0);
        let mut written = 0;
        while written < bytes {
            let n = std::cmp::min(bytes - written, buf.len());
            self.write(&buf[..n]).await?;
            written += n;
        }
        Ok(())
    }

    /// modify the program headers and insert the kmsg log
    async fn insert_kmsg_note(&mut self, buf: &mut [u8]) -> anyhow::Result<()> {
        // elf header
        let mut ehdr: Elf64_Ehdr = Elf64_Ehdr::new_zeroed();
        self.read(ehdr.as_mut_bytes(), true).await;
        self.write(ehdr.as_bytes()).await?;

        tracing::trace!("ehdr: {:#x?}", &ehdr);

        // file must not contain sections headers, as that is not handled
        if ehdr.e_shoff != 0 {
            tracing::error!("Dump contains section headers, which are not supported");
        }

        // notes program header
        let mut notes_phdr: Elf64_Phdr = Elf64_Phdr::new_zeroed();
        self.read(notes_phdr.as_mut_bytes(), true).await;

        tracing::trace!("initial notes_phdr: {:#x?}", notes_phdr);
        if notes_phdr.p_type != PT_NOTE {
            tracing::error!("Expected type {:#x}, got {:#x}", PT_NOTE, notes_phdr.p_type);
        }
        let initial_notes_size = notes_phdr.p_filesz as usize;
        notes_phdr.p_filesz += KMSG_NOTE_BYTES as u64;
        tracing::trace!("modified notes_phdr: {:#x?}", notes_phdr);

        self.write(notes_phdr.as_bytes()).await?;

        // remaining program headers
        let mut phnum_remaining = ehdr.e_phnum as usize - 1;
        let max = buf.len() / size_of::<Elf64_Phdr>();
        while phnum_remaining > 0 {
            let phnum = std::cmp::min(phnum_remaining, max);
            let phdrs_size = phnum * size_of::<Elf64_Phdr>();
            self.read(&mut buf[..phdrs_size], true).await;
            let phdrs: &mut [Elf64_Phdr] =
                <[Elf64_Phdr]>::mut_from_bytes(&mut buf[..phdrs_size]).unwrap();

            tracing::trace!("initial phdrs: {:#x?}", phdrs);
            for phdr in &mut phdrs[..] {
                phdr.p_offset += KMSG_NOTE_BYTES as u64;
            }
            tracing::trace!("modified phdrs: {:#x?}", phdrs);

            self.write(&buf[..phdrs_size]).await?;
            phnum_remaining -= phnum;
        }

        // we don't need to modify the other notes, so just stream them
        let padding_before_notes = notes_phdr.p_offset as usize - self.read_bytes_total;
        tracing::trace!("padding_before_notes: {:#x?}", padding_before_notes);
        self.stream_n(buf, padding_before_notes + initial_notes_size)
            .await?;

        // create the note name and header
        let name = b"KMSG\0\0\0\0";
        let header_and_name = size_of::<Elf64_Nhdr>() + name.len();
        let kmsg_header = Elf64_Nhdr {
            namesz: 5,
            descsz: (KMSG_NOTE_BYTES - header_and_name) as u32,
            ntype: 0xffffffff,
        };

        // save space for header, name, and length
        let max_kmsg_len = KMSG_NOTE_BYTES - header_and_name - size_of::<u32>();

        // open the kmsg as a nonblocking file
        let mut kmsg = fs_err::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open("/dev/kmsg")?;

        self.write(kmsg_header.as_bytes()).await?;
        self.write(name).await?;
        let kmsg_len = self.stream_file(buf, &mut kmsg, max_kmsg_len).await;
        self.write_padding(buf, max_kmsg_len - kmsg_len).await?;
        // write the actual length of the kmsg log in a predictable location
        self.write((kmsg_len as u32).as_bytes()).await?;

        tracing::debug!(len = kmsg_len, "wrote kmsg");

        Ok(())
    }

    /// Let the VSP know that is all the data so the host can start reporting
    async fn complete(&mut self, os_version: &OsVersionInfo) -> anyhow::Result<()> {
        tracing::debug!(
            "Read {} bytes, wrote {} bytes",
            self.read_bytes_total,
            self.wrote_bytes_total
        );
        if self.read_bytes_total + KMSG_NOTE_BYTES == self.wrote_bytes_total {
            tracing::debug!(
                "Bytes written includes {} bytes for kmsg note",
                KMSG_NOTE_BYTES,
            );
        } else {
            tracing::error!(
                "wrote - read = {}, expected {} to account for kmsg note",
                self.wrote_bytes_total as isize - self.read_bytes_total as isize,
                KMSG_NOTE_BYTES,
            );
        }

        let dump_complete = crash::DumpCompleteRequestV1 {
            header: make_header(
                Some(&self.header),
                crash::MessageType::REQUEST_NIX_DUMP_COMPLETE_V1,
            ),
            info: crash::CompletionInfoV1 {
                major_version: os_version.major(),
                minor_version: os_version.minor(),
                version_banner: *os_version.banner(),
                vtl: 2,
            },
        };

        self.writer.send(dump_complete.as_bytes()).await?;

        Ok(())
    }

    fn wrote_bytes_total(&self) -> usize {
        self.wrote_bytes_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::Cursor;
    use pal_async::async_test;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use vmbus_async::pipe::connected_message_pipes;
    use vmbus_channel::connected_async_channels;
    use vmbus_ring::FlatRingMem;
    use vmbus_ring::Ring;

    // Large enough for the biggest message: a full dump data payload.
    // Tests use the second pipe as the host because only that endpoint
    // signals channel closure to its peer when dropped.
    const RING_SIZE: usize = 0x10000;
    const HOST_ERROR: i32 = 0x8000_4005_u32 as i32;

    fn os_version() -> OsVersionInfo {
        OsVersionInfo {
            banner: [0; 256],
            major_minor: (6, 1),
        }
    }

    fn test_dump(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    /// Answers the capabilities, config, and dump-start requests successfully.
    async fn host_negotiate(host: &mut MessagePipe<FlatRingMem>) {
        let (mut reader, mut writer) = host.split();

        let rq: crash::DumpCapabilitiesRequestV1 = read_message(&mut reader).await.unwrap();
        let resp = crash::DumpCapabilitiesResponseV1 {
            header: make_header(
                Some(&rq.header),
                crash::MessageType::RESPONSE_GET_CAPABILITIES_V1,
            ),
            capabilities: crash::Capabilities::new().with_linux_config_v1(true),
        };
        writer.send(resp.as_bytes()).await.unwrap();

        let rq: crash::DumpConfigRequestV1 = read_message(&mut reader).await.unwrap();
        let resp = crash::DumpConfigResponseV1 {
            header: make_header(
                Some(&rq.header),
                crash::MessageType::RESPONSE_GET_NIX_DUMP_CONFIG_V1,
            ),
            config: crash::ConfigV1 {
                max_dump_size: 1 << 30,
                dump_type: crash::DumpType::ELF,
            },
        };
        writer.send(resp.as_bytes()).await.unwrap();

        let rq: crash::DumpStartRequestV1 = read_message(&mut reader).await.unwrap();
        let resp = crash::DumpStartResponseV1 {
            header: make_header(
                Some(&rq.header),
                crash::MessageType::RESPONSE_NIX_DUMP_START_V1,
            ),
            status: 0,
        };
        writer.send(resp.as_bytes()).await.unwrap();
    }

    /// Receives the next dump-write request header from the guest.
    async fn host_recv_write_request(
        reader: &mut MessageReadHalf<'_, FlatRingMem>,
    ) -> crash::DumpWriteRequestV1 {
        read_message(reader).await.unwrap()
    }

    // Test: Transfer a multi-message dump to a host that accepts every write.
    // Expected: send_dump succeeds and the host receives every byte in order,
    //       followed by the completion request.
    #[async_test]
    async fn send_dump_succeeds_when_host_accepts_dump() {
        let (guest, mut host) = connected_message_pipes(RING_SIZE);
        let dump = test_dump(100_000);
        let mut source = Cursor::new(dump.clone());
        let os_version = os_version();

        let host_task = async {
            host_negotiate(&mut host).await;
            let (mut reader, _writer) = host.split();
            let mut received = Vec::new();
            let mut buf = vec![0; crash::MAX_DUMP_PAYLOAD_SIZE];
            loop {
                let n = reader.recv(&mut buf).await.unwrap();
                let header = Header::read_from_prefix(&buf[..n]).unwrap().0;
                match header.message_type {
                    crash::MessageType::REQUEST_NIX_DUMP_WRITE_V1 => {
                        let rq = crash::DumpWriteRequestV1::read_from_bytes(&buf[..n]).unwrap();
                        let (offset, size) = (rq.offset, rq.size);
                        assert_eq!(offset as usize, received.len());
                        let n = reader.recv(&mut buf).await.unwrap();
                        assert_eq!(n, size as usize);
                        received.extend_from_slice(&buf[..n]);
                    }
                    crash::MessageType::REQUEST_NIX_DUMP_COMPLETE_V1 => break,
                    ty => panic!("unexpected message type {ty:?}"),
                }
            }
            received
        };

        let (result, received) =
            futures::join!(send_dump(guest, &mut source, &os_version, false), host_task);

        result.unwrap();
        assert!(received == dump, "host received a different dump");
    }

    // Test: The host rejects a dump write with a nonzero status while the
    //       guest is still streaming.
    // Expected: send_dump returns an error instead of reporting success.
    #[async_test]
    async fn send_dump_fails_when_host_reports_write_error() {
        let (guest, mut host) = connected_message_pipes(RING_SIZE);
        // Much larger than the ring, so streaming cannot finish while the
        // host stops reading.
        let mut source = Cursor::new(test_dump(1 << 20));
        let os_version = os_version();

        let host_task = async {
            host_negotiate(&mut host).await;
            let (mut reader, mut writer) = host.split();
            let rq = host_recv_write_request(&mut reader).await;
            let resp = crash::DumpWriteResponseV1 {
                header: make_header(
                    Some(&rq.header),
                    crash::MessageType::RESPONSE_NIX_DUMP_WRITE_V1,
                ),
                status: HOST_ERROR,
            };
            writer.send(resp.as_bytes()).await.unwrap();
        };

        let (result, ()) =
            futures::join!(send_dump(guest, &mut source, &os_version, false), host_task);

        let err = result.expect_err("host write error was reported as success");
        assert!(
            format!("{err:#}").contains("Host error 0x80004005"),
            "unexpected error: {err:#}"
        );
    }

    // Test: The host closes the channel after the first dump write.
    // Expected: send_dump returns an error instead of reporting success.
    #[async_test]
    async fn send_dump_fails_when_host_closes_channel() {
        let (guest, mut host) = connected_message_pipes(RING_SIZE);
        let mut source = Cursor::new(test_dump(1 << 20));
        let os_version = os_version();

        let host_task = async move {
            host_negotiate(&mut host).await;
            let (mut reader, _writer) = host.split();
            host_recv_write_request(&mut reader).await;
            // Dropping the host pipe closes the channel.
        };

        let (result, ()) =
            futures::join!(send_dump(guest, &mut source, &os_version, false), host_task);

        result.expect_err("closed channel was reported as success");
    }

    /// An unaligned outgoing consumer offset fails streaming when the writer
    /// exhausts its cached space. The host stays open and sends no error.
    #[async_test]
    async fn send_dump_propagates_streaming_error() {
        let (guest, host) = connected_async_channels(RING_SIZE);
        let outgoing = guest.out_ring.mem().clone();
        let guest = MessagePipe::new(guest).unwrap();
        let mut host = MessagePipe::new(host).unwrap();
        let mut source = Cursor::new(test_dump(1 << 20));
        let os_version = os_version();

        let host_task = async {
            host_negotiate(&mut host).await;
            outgoing.control()[1].store(1, Ordering::Relaxed);
        };

        let (result, ()) =
            futures::join!(send_dump(guest, &mut source, &os_version, false), host_task);

        let error = result.expect_err("failed streaming was reported as success");
        assert_eq!(error.to_string(), "failed to stream dump");
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }

    /// The dump fits in the writer's cached ring space, but completion does not.
    /// Refreshing space then detects the invalid consumer offset, without a
    /// read-side failure or host error masking the completion error.
    #[async_test]
    async fn send_dump_propagates_completion_error() {
        let (guest, host) = connected_async_channels(4096);
        let outgoing = guest.out_ring.mem().clone();
        let guest = MessagePipe::new(guest).unwrap();
        let mut host = MessagePipe::new(host).unwrap();
        let mut source = Cursor::new(test_dump(3800));
        let os_version = os_version();

        let host_task = async {
            host_negotiate(&mut host).await;
            outgoing.control()[1].store(1, Ordering::Relaxed);
        };

        let (result, ()) =
            futures::join!(send_dump(guest, &mut source, &os_version, false), host_task);

        let error = result.expect_err("failed completion was reported as success");
        assert_eq!(error.to_string(), "failed to complete dump");
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }
}
