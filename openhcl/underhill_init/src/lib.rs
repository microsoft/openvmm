// Copyright (C) Microsoft Corporation. All rights reserved.

//! This module implements the the Underhill initial process.

#![cfg(target_os = "linux")]
// UNSAFETY: Calling libc functions to set up global system state.
#![allow(unsafe_code)]

mod options;
mod syslog;

// `pub` so that the missing_docs warning fires for options without
// documentation.
pub use options::Options;

use anyhow::Context;
use libc::c_void;
use libc::STDERR_FILENO;
use libc::STDIN_FILENO;
use libc::STDOUT_FILENO;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ffi::OsStr;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::os::unix::prelude::*;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use syslog::SysLog;
use walkdir::WalkDir;

const UNDERHILL_PATH: &str = "/bin/underhill";

struct FilesystemMount<'a> {
    source: &'a CStr,
    target: &'a CStr,
    fstype: &'a CStr,
    options: &'a CStr,
    flags: u64,
}

impl<'a> FilesystemMount<'a> {
    pub fn new(
        source: &'a CStr,
        target: &'a CStr,
        fstype: &'a CStr,
        flags: u64,
        options: &'a CStr,
    ) -> Self {
        Self {
            source,
            target,
            fstype,
            options,
            flags,
        }
    }

    pub fn mount(&self) -> io::Result<()> {
        // SAFETY: calling the API according to the documentation
        let err = unsafe {
            libc::mount(
                self.source.as_ptr(),
                self.target.as_ptr(),
                self.fstype.as_ptr(),
                self.flags,
                self.options.as_ptr().cast::<c_void>(),
            )
        };

        if err != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

mod dev_random_ioctls {
    pub const MAX_ENTROPY_SIZE: usize = 256;
    const RANDOM_IOC_MAGIC: u8 = b'R';
    #[repr(C)]
    pub struct RndAddEntropy {
        pub entropy_count: i32,
        pub buf_size: i32,
        pub buf: [u8; MAX_ENTROPY_SIZE],
    }
    // RNDADDENTROPY _IOW( 'R', 0x03, int [2] )
    nix::ioctl_write_ptr_bad!(
        rnd_add_entropy_ioctl,
        nix::request_code_write!(RANDOM_IOC_MAGIC, 0x3, size_of::<std::os::raw::c_int>() * 2),
        RndAddEntropy
    );
}

// If it is available, use host-generated entropy to speed up boot and
// improve entropy quality.
//
// This is especially useful on machines without hardware random number
// generation. When random numbers are requested from /dev/random, the
// system blocks until it has gained enough entropy.
//
// It is safe to apply host-provided entropy even to a confidential VM,
// because host-provided data is hashed into the existing entropy sources.
fn use_host_entropy() -> anyhow::Result<()> {
    use dev_random_ioctls::MAX_ENTROPY_SIZE;

    let host_entropy = match fs_err::read("/proc/device-tree/openhcl/entropy/reg") {
        Ok(contents) => contents,
        Err(e) => {
            log::warn!("Did not get entropy from the host: {e:#}");
            return Ok(());
        }
    };

    if host_entropy.len() > MAX_ENTROPY_SIZE {
        log::warn!(
            "Truncating host-provided entropy (received {} bytes)",
            host_entropy.len()
        );
    }
    let use_entropy_bytes = std::cmp::min(host_entropy.len(), MAX_ENTROPY_SIZE);
    log::info!("Using {} bytes of entropy from the host", use_entropy_bytes);

    let mut entropy = dev_random_ioctls::RndAddEntropy {
        entropy_count: (use_entropy_bytes * 8) as i32,
        buf_size: use_entropy_bytes as i32,
        buf: [0; MAX_ENTROPY_SIZE],
    };
    entropy.buf[..use_entropy_bytes].copy_from_slice(&host_entropy[..use_entropy_bytes]);

    let dev_random = fs_err::OpenOptions::new()
        .write(true)
        .open("/dev/random")
        .with_context(|| ("failed to open dev random for setting entropy").to_string())?;

    // SAFETY: API called according to the documentation.
    unsafe {
        dev_random_ioctls::rnd_add_entropy_ioctl(dev_random.as_raw_fd(), &entropy)
            .context("rnd_add_entropy_ioctl")?;
    }

    Ok(())
}

fn setup(
    stat_files: &[&str],
    options: &Options,
    writes: &[(&str, &str)],
    filesystems: &[FilesystemMount<'_>],
) -> anyhow::Result<()> {
    log::info!("Mounting filesystems");

    for filesystem in filesystems {
        let path: &Path = OsStr::from_bytes(filesystem.target.to_bytes()).as_ref();
        // Ensure the target exists.
        fs_err::create_dir_all(path)?;

        filesystem
            .mount()
            .with_context(|| format!("failed to mount {}", path.display()))?;
    }

    log::info!("Command line args: {:?}", options);

    if log::log_enabled!(log::Level::Trace) {
        for stat_file in stat_files {
            if let Ok(file) = fs_err::File::open(stat_file) {
                log::trace!("{}", stat_file);
                for line in BufReader::new(file).lines() {
                    if let Ok(line) = line {
                        log::trace!("{}", line);
                    }
                }
            }
        }
    }

    log::info!("Setting system resource limits and parameters");

    for (path, data) in writes {
        fs_err::write(path, data).with_context(|| format!("failed to write {data}"))?;
    }

    use_host_entropy().context("use host entropy")?;

    for setup in &options.setup_script {
        log::info!("Running provided setup script {}", setup);

        let result = Command::new("/bin/sh")
            .arg("-c")
            .arg(setup)
            .stderr(Stdio::inherit())
            .output()
            .context("script failed to start")?;

        if !result.status.success() {
            anyhow::bail!("setup script failed: {}", result.status);
        }

        // Capture key-value pairs in the script's stdout as environment
        // variables.
        for line in result.stdout.split(|&x| x == b'\n') {
            if let Some((key, value)) = std::str::from_utf8(line)
                .ok()
                .and_then(|line| line.split_once('='))
            {
                log::info!("setting env var {}={}", key, value);
                std::env::set_var(key, value);
            }
        }
    }
    Ok(())
}

fn run(options: &Options) -> anyhow::Result<()> {
    let mut command = Command::new(UNDERHILL_PATH);
    command.arg("--pid").arg("/run/underhill.pid");
    command.args(&options.underhill_args);

    // Update the file descriptor limit for the main process, since large VMs
    // require lots of fds. There is no downside to a larger value except that
    // we may less effectively catch fd leaks (which have not historically been
    // a problem). So use a value that is plenty large enough for any VM.
    let limit = 0x100000;
    // SAFETY: calling according to docs.
    unsafe {
        if libc::prlimit(
            0,
            libc::RLIMIT_NOFILE,
            &libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            },
            std::ptr::null_mut(),
        ) < 0
        {
            return Err(io::Error::last_os_error()).context("failed to update rlimit");
        }
    }

    log::info!("running {:?}", &command);

    let child = command.spawn().context("underhill failed to start")?;

    let status = reap_until(child).context("wait failed")?;
    if status.success() {
        log::info!("underhill exited successfully");
    } else {
        log::error!("underhill terminated unsuccessfully: {}", status);
    }

    std::process::exit(status.code().unwrap_or(255));
}

/// Reap zombie processes until `child` exits. Return `child`'s exit status.
fn reap_until(child: Child) -> io::Result<ExitStatus> {
    loop {
        let mut status = 0;
        // SAFETY: calling according to docs.
        let pid = unsafe { libc::wait(&mut status) };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }

        if pid == child.id() as i32 {
            // The child process died. Pass through the exit status.
            return Ok(ExitStatus::from_raw(status));
        }
    }
}

fn move_stdio(src: impl Into<std::fs::File>, dst: RawFd) {
    assert!((0..=2).contains(&dst));
    let src = src.into();
    if src.as_raw_fd() != dst {
        // SAFETY: calling as documented.
        let r = unsafe { libc::dup2(src.as_raw_fd(), dst) };
        assert_eq!(r, dst);
    } else {
        let _ = src.into_raw_fd();
    }
}

fn init_logging() {
    // Open /dev/null for replacing stdin and stdout.
    move_stdio(fs_err::File::open("/dev/null").unwrap(), STDIN_FILENO);

    move_stdio(
        fs_err::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap(),
        STDOUT_FILENO,
    );

    // Set stderr to /dev/ttyprintk to catch panic stack.
    let ttyprintk_err = match fs_err::OpenOptions::new()
        .write(true)
        .open("/dev/ttyprintk")
    {
        Ok(ttyprintk) => {
            move_stdio(ttyprintk, STDERR_FILENO);
            None
        }
        Err(err) => Some(err),
    };

    // Set the log output to use /dev/kmsg directly.
    let syslog = SysLog::new().expect("failed to open /dev/kmsg");
    log::set_boxed_logger(Box::new(syslog)).expect("no logger already set");

    // TODO: syslog should respect the HVLITE_LOG env variable to allow runtime
    // log level changes without rebuilding, but for now downgrade the default
    // to info to stop noisy logs and allow compile time changes for local
    // debugging.
    log::set_max_level(log::LevelFilter::Info);

    // Now that logging is initialized, fail if opening ttyprintk failed.
    // Otherwise, we probably won't see the failure reason in the logs.
    if let Some(err) = ttyprintk_err {
        log::error!("failed to open stderr output: {}", err);
        panic!();
    }
}

fn load_modules(modules_path: &str) -> anyhow::Result<()> {
    // Get the kernel command line.
    let cmdline = fs_err::read_to_string("/proc/cmdline")?;
    let mut params = HashMap::new();
    for option in cmdline.split_ascii_whitespace() {
        if let Some((module, option)) = option.split_once('.') {
            if option.contains('=') {
                let v: &mut String = params.entry(module.replace('-', "_")).or_default();
                *v += option;
                *v += " ";
            }
        }
    }

    // Load the modules.
    for module in WalkDir::new(modules_path).sort_by_file_name() {
        let module = module?;
        if !module.file_type().is_file() {
            continue;
        }

        let module = module.path();
        let module_name = module
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .replace('-', "_");

        let params = params.get_mut(&module_name);

        log::info!(
            "loading kernel module {}: {}",
            module.display(),
            params.as_ref().map_or("", |s| s.as_str())
        );
        let file = fs_err::File::open(module).context("failed to open module")?;

        let params = if let Some(params) = params {
            // Null terminate
            params.pop();
            params.push('\0');
            params.as_bytes()
        } else {
            b"\0"
        };

        // SAFETY: calling the syscall as documented. Of course, the module
        // being loaded has full kernel privileges, but the contents of the file
        // system are trusted.
        let r =
            unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), params.as_ptr(), 0) };
        if r < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("failed to load module {}", module.display()));
        }

        log::info!("load complete for {}", module.display());
    }

    // Once the kernel modules are loaded into memory, the module files are not needed anymore.
    // By deleting them after, we can save some memory.
    fs_err::remove_dir_all(modules_path)?;

    Ok(())
}

fn timestamp() -> u64 {
    let mut tp;
    // SAFETY: calling `clock_gettime` as documented.
    unsafe {
        tp = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut tp);
    }
    Duration::new(tp.tv_sec as u64, tp.tv_nsec as u32).as_nanos() as u64
}

fn do_main() -> anyhow::Result<()> {
    let boot_time = timestamp();
    std::env::set_var("KERNEL_BOOT_TIME", boot_time.to_string());

    init_logging();

    log::info!(
        "Initial process: crate_name={}, crate_revision={}, crate_branch={}",
        env!("CARGO_PKG_NAME"),
        option_env!("VERGEN_GIT_SHA").unwrap_or("UNKNOWN_REVISION"),
        option_env!("VERGEN_GIT_BRANCH").unwrap_or("UNKNOWN_BRANCH"),
    );

    let stat_files = [
        "/proc/uptime",
        "/proc/timer_list",
        "/proc/interrupts",
        "/proc/meminfo",
        "/proc/iomem",
        "/proc/ioports",
        "/proc/sys/kernel/pid_max",
        "/proc/sys/kernel/threads-max",
        "/proc/sys/vm/max_map_count",
    ];
    let options = Options::parse();
    let writes = &[
        // The kernel sets the maximum number of threads to a number
        // inferred from the size of RAM: the thread structures must
        // occupy only 1/8th of the available RAM pages. That is quite
        // small for Underhill in the interactive mode so the kernel
        // would allow only a small number of threads which doesn't
        // let the interactive mode run.
        ("/proc/sys/kernel/threads-max", "32768"),
        // Censor kernel pointers in the logs for security
        ("/proc/sys/kernel/kptr_restrict", "1"),
        // Enable transparent hugepages on requested VMAs. This is used to map
        // VTL0 memory with huge pages. Although this is on by default in our
        // kernel configuration, the kernel turns it off for low-memory systems
        // (which VTL2 is).
        ("/sys/kernel/mm/transparent_hugepage/enabled", "madvise"),
        // Configure the vmbus devices to be handled as user-mode vmbus
        // driver.
        (
            "/sys/bus/vmbus/drivers/uio_hv_generic/new_id",
            // GET
            "8dedd1aa-9056-49e4-bfd6-1bf90dc38ef0",
        ),
        (
            "/sys/bus/vmbus/drivers/uio_hv_generic/new_id",
            // UART
            "8b60ccf6-709f-4c11-90b5-229c959a9e6a",
        ),
        (
            "/sys/bus/vmbus/drivers/uio_hv_generic/new_id",
            // Crashdump
            "427b03e7-4ceb-4286-b5fc-486f4a1dd439",
        ),
        (
            "/proc/sys/kernel/core_pattern",
            if underhill_confidentiality::confidential_filtering_enabled() {
                // Disable the processing of dumps for CVMs.
                ""
            } else {
                // When a user mode crash occurs, the kernel will call `/bin/underhill-crash`
                // passing the information of the crashing process to it.
                // The order of these arguments must match exactly with the order
                // that underhill_crash is expecting.
                "|/bin/underhill-crash %p %i %s %e"
            },
        ),
        // Handle one crashing process at a time.
        ("/proc/sys/kernel/core_pipe_limit", "1"),
        // Don't bother OOM killing processes when out of memory, just panic.
        // Any unexpected process termination is a fatal error anyway, so panic
        // to get a VM crash dump.
        ("/proc/sys/vm/panic_on_oom", "1"),
        // Set the min watermark to 1MiB, the minimum value recommended in
        // Documentation/admin-guide/sysctl/vm.rst (Linux kernel). This controls kswapd.
        // kswapd reclaims memory by swapping or dropping reclaimable caches when the
        // number of free pages in a zone is below the low watermark.
        // VTL2 has no swap and has no reclaimable caches, so there is nothing it can do
        // if it is invoked. By setting the watermarks as low as possible, we
        // ensure that it won't be invoked in normal operation (if it does get invoked, the system
        // is probably about to OOM anyway).
        // This also indirectly controls the size of the percpu pagesets.
        // We want to keep that size as small as possible without introducing contention on the
        // zone lock, as these pages are:
        // * Not counted in MemFree of /proc/meminfo
        // * Not considered when determining if kswapd should be started
        ("/proc/sys/vm/min_free_kbytes", "1024"),
        // Make the high and low watermark as close to the min watermark as possible. This value's
        // units are fractions of 10,000. This means the watermarks will be spaced 0.01% of available
        // memory apart.
        ("/proc/sys/vm/watermark_scale_factor", "1"),
        // Disable the watermark boost feature
        ("/proc/sys/vm/watermark_boost_factor", "0"),
    ];
    let filesystems = [
        FilesystemMount::new(
            c"proc",
            c"/proc",
            c"proc",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RELATIME,
            c"",
        ),
        FilesystemMount::new(
            c"sysfs",
            c"/sys",
            c"sysfs",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RELATIME,
            c"",
        ),
        FilesystemMount::new(
            c"dev",
            c"/dev",
            c"devtmpfs",
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_RELATIME,
            c"",
        ),
        FilesystemMount::new(
            c"devpts",
            c"/dev/pts",
            c"devpts",
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_RELATIME,
            c"",
        ),
    ];

    setup(&stat_files, &options, writes, &filesystems)?;

    if matches!(
        std::env::var("UNDERHILL_NVME_VFIO").as_deref(),
        Ok("true" | "1")
    ) {
        // Register VFIO to bind to all NVMe devices, from any vendor.
        //
        // Since nvme is loaded as a module, and that happens after this call,
        // this will take precedence over the in-kernel nvme driver.
        fs_err::write(
            "/sys/bus/pci/drivers/vfio-pci/new_id",
            "ffffffff ffffffff ffffffff ffffffff 010802 ffffff",
        )
        .context("failed to register nvme for vfio")?;
        log::info!("registered vfio-pci as driver for nvme");
    }

    // Start loading modules in parallel.
    std::thread::spawn(|| {
        if let Err(err) = load_modules("/lib/modules") {
            panic!("failed to load modules: {:#}", err);
        }
    });

    run(&options)
}

pub fn main() -> ! {
    match do_main() {
        Ok(_) => unreachable!(),
        Err(err) => {
            log::error!("fatal: {:#}", err);
            std::process::exit(1);
        }
    }
}
