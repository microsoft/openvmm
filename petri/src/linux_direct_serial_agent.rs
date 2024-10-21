// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use pal_async::socket::ReadHalf;
use pal_async::socket::WriteHalf;
use unix_socket::UnixStream;

const BUSYBOX_INIT: &str =
    "/bin/busybox --install /bin && mount none /dev -t devtmpfs && mount none /proc -t proc && mount none /sys -t sysfs";

pub(crate) struct LinuxDirectSerialAgent {
    /// Writer to serial 0, the console we define in our kernel commandline
    write: WriteHalf<UnixStream>,
    /// Reader on serial 1, not serial 0, to avoid reading the commands we just sent
    read: ReadHalf<UnixStream>,
    /// Delayed initialization so new can be synchronous
    init: bool,
}

impl LinuxDirectSerialAgent {
    pub(crate) fn new(
        serial1_read: ReadHalf<UnixStream>,
        serial0_write: WriteHalf<UnixStream>,
    ) -> Self {
        Self {
            read: serial1_read,
            write: serial0_write,
            init: false,
        }
    }
}

impl LinuxDirectSerialAgent {
    // Inform the agent that it should reinitialize itself on the next command.
    pub(crate) fn reset(&mut self) {
        self.init = false;
    }

    pub(crate) async fn run_command(&mut self, command: &str) -> anyhow::Result<String> {
        self.init_busybox_if_necessary().await?;
        let bytes = self.run_command_core(command).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn run_command_core(&mut self, command: &str) -> anyhow::Result<Vec<u8>> {
        // We need a signal that the current command has finished executing so that we can stop reading
        // and return to the caller. The pipe will remain open, so we can't just read until we get 0 bytes.
        // Instead we send this special text sequence to signal the end of the command, since it's unlikely
        // that a normal command will ever output it.
        const COMMAND_END_SIGNAL: &str = "== Petri Command Complete ==";
        let command = format!("({command}) > /dev/ttyS1\necho {COMMAND_END_SIGNAL} > /dev/ttyS1\n");

        // When reading the output there will be a trailing newline.
        const COMMAND_END_SIGNAL_READ: &str = "== Petri Command Complete ==\r\n";

        self.write.write_all(command.as_bytes()).await?;

        let mut output = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = self.read.read(&mut buf).await?;
            tracing::debug!(buf = ?&buf[..n], "read serial bytes from guest");
            output.extend_from_slice(&buf[..n]);
            if output.ends_with(COMMAND_END_SIGNAL_READ.as_bytes()) {
                output.truncate(output.len() - COMMAND_END_SIGNAL_READ.len());
                break;
            }
        }

        Ok(output)
    }

    async fn init_busybox_if_necessary(&mut self) -> anyhow::Result<()> {
        if !self.init {
            self.run_command_core(BUSYBOX_INIT).await?;
            self.init = true;
        };
        Ok(())
    }
}
