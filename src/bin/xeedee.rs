use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use indicatif::ProgressBar;
use owo_colors::OwoColorize;
use rootcause::prelude::*;
use tabled::Tabled;

mod ui;

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use xeedee::commands::AltAddr;
use xeedee::commands::Breakpoint;
use xeedee::commands::DataBreakKind;
use xeedee::commands::DataBreakpoint;
use xeedee::commands::DbgName;
use xeedee::commands::Delete;
use xeedee::commands::DirList;
use xeedee::commands::DmVersion;
use xeedee::commands::DriveFreeSpace;
use xeedee::commands::DriveList;
use xeedee::commands::FileEof;
use xeedee::commands::FileUploadKind;
use xeedee::commands::GetConsoleFeatures;
use xeedee::commands::GetConsoleMem;
use xeedee::commands::GetConsoleType;
use xeedee::commands::GetFileAttributes;
use xeedee::commands::GetFileRange;
use xeedee::commands::GetMem;
use xeedee::commands::GetNetAddrs;
use xeedee::commands::GetPid;
use xeedee::commands::GetSocketInfo;
use xeedee::commands::IsStopped;
use xeedee::commands::MakeDirectory;
use xeedee::commands::ModuleSections;
use xeedee::commands::Modules;
use xeedee::commands::PerfCounterList;
use xeedee::commands::QueryPerfCounter;
use xeedee::commands::Reboot;
use xeedee::commands::RebootFlags;
use xeedee::commands::Rename;
use xeedee::commands::SetMem;
use xeedee::commands::SysTime;
use xeedee::commands::ThreadId;
use xeedee::commands::ThreadInfo;
use xeedee::commands::Threads;
use xeedee::commands::Title;
use xeedee::commands::WalkMem;
use xeedee::commands::XbeInfo;
#[cfg(feature = "dangerous")]
use xeedee::commands::dangerous::drivemap as dm;
#[cfg(feature = "capture")]
use xeedee::commands::pix::CaptureSession;
#[cfg(feature = "capture")]
use xeedee::commands::pix::Notification;
use xeedee::error::Error;
use xeedee::transport::CaptureLog;
use xeedee::transport::RecordingTransport;

use xeedee::Client;
use xeedee::XBDM_PORT;
use xeedee::discovery::DiscoveryConfig;
use xeedee::discovery::NAP_PORT;
use xeedee::discovery::discover_all;
use xeedee::discovery::find_by_name;
use xeedee::transport::tokio::Target;
use xeedee::transport::tokio::connect_target_timeout;

#[derive(Parser, Debug)]
#[command(
    name = "xeedee",
    about = "Async-first XBDM (Xbox Debug Monitor) client",
    version
)]
struct Cli {
    /// Host, IP, or `host:port` of the target XBDM console.
    ///
    /// Accepts `192.168.1.26`, `192.168.1.26:730`, `deanxbox`,
    /// `deanxbox:730`, `[fe80::1]:730`, or a bare IPv6 literal. Not
    /// required for the `discover` and `resolve` subcommands.
    #[arg(short = 'H', long, env = "XEEDEE_HOST", global = true)]
    host: Option<String>,

    /// Port override. Ignored when `--host` already carries a port.
    #[arg(short = 'p', long, default_value_t = XBDM_PORT, env = "XEEDEE_PORT")]
    port: u16,

    /// Connection timeout in seconds.
    #[arg(long, default_value_t = 5)]
    timeout: u64,

    /// Record the byte-level conversation to this file for later replay
    /// into tests via `MockTransport`.
    #[arg(long, value_name = "PATH")]
    capture: Option<PathBuf>,

    /// Tracing filter (e.g. `xeedee=debug`). Defaults to `info`.
    #[arg(long, default_value = "info")]
    log: String,

    /// Disable progress bars on transfers. Automatically disabled when
    /// stderr isn't a TTY.
    #[arg(long, global = true)]
    no_progress: bool,

    /// Table output format.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Pretty)]
    format: OutputFormat,

    /// Report file sizes as raw byte counts instead of humanised units
    /// (`1.23 MiB`). Memory/address sizes are always shown as hex.
    #[arg(long, global = true)]
    bytes: bool,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    /// Tab-separated values. Parse-friendly, no color, no borders.
    Plain,
    /// Boxed tables with light color accents via `tabled` + `owo-colors`.
    Pretty,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum LsSortBy {
    /// Alphabetical by entry name (case-insensitive).
    Name,
    /// Numeric by file size. Directories compare as zero.
    Size,
    /// Group by kind: directories first (or last, depending on order).
    Kind,
    /// Last-change timestamp.
    Modified,
    /// Creation timestamp.
    Created,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum SortOrder {
    Asc,
    Desc,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Broadcast an XBDM NAP discovery probe and list all consoles that
    /// reply within the listen window.
    Discover {
        /// Listen for replies for this many milliseconds after the probe.
        #[arg(long, default_value_t = 1500)]
        listen_ms: u64,
        /// Override the broadcast destination. Defaults to
        /// `255.255.255.255:730`.
        #[arg(long)]
        broadcast: Option<String>,
    },

    /// Resolve a named console to its IP via the NAP lookup protocol.
    Resolve {
        /// Console `dbgname` to resolve.
        name: String,
        /// Listen window in milliseconds.
        #[arg(long, default_value_t = 1500)]
        listen_ms: u64,
    },

    /// Open a connection, read the banner, and disconnect. Useful for
    /// validating reachability and generating an initial capture.
    Ping,

    /// Read or set the debuggable name of the console.
    Dbgname {
        #[arg(long)]
        set: Option<String>,
    },

    /// Report the console's clock as both a FILETIME and a SystemTime.
    Systime,

    /// Report the XBDM build identifier.
    Dmversion,

    /// Report the hardware kit type (retail/devkit/testkit).
    Consoletype,

    /// List the feature flags advertised by the kernel.
    Consolefeatures,

    /// List accessible drives.
    Drivelist,

    /// Report free/total bytes on a drive.
    Df {
        /// Drive root, e.g. `DEVKIT:\` or `E:\`.
        drive: String,
    },

    /// List a directory's entries.
    Ls {
        /// Remote path, e.g. `DEVKIT:\\`.
        path: String,

        /// Field to sort by.
        #[arg(long, value_enum, default_value_t = LsSortBy::Name)]
        sort_by: LsSortBy,

        /// Sort direction.
        #[arg(long, value_enum, default_value_t = SortOrder::Desc)]
        sort_order: SortOrder,
    },

    /// Recursively enumerate a directory tree. With no path, walks every
    /// drive returned by `drivelist`.
    Tree {
        /// Remote path (e.g. `DEVKIT:\\`). Omit to walk every drive.
        path: Option<String>,
        /// Maximum recursion depth. 0 means unlimited.
        #[arg(long, default_value_t = 0u32)]
        max_depth: u32,
        /// Include files in the output (default is directories-only, a
        /// la `tree -d`). Use `--files` to include leaf files too.
        #[arg(long)]
        files: bool,
    },

    /// Show metadata for a remote file or directory.
    Stat {
        /// Remote path.
        path: String,
    },

    /// Create a remote directory.
    Mkdir { path: String },

    /// Delete a remote file (or directory with `--dir`).
    Rm {
        path: String,
        #[arg(long)]
        dir: bool,
    },

    /// Rename / move a remote path.
    Mv { from: String, to: String },

    /// Download a file from the console.
    Get {
        /// Remote path to download.
        remote: String,
        /// Local file to write to, or `-` for stdout.
        #[arg(short, long, default_value = "-")]
        output: String,
        /// Optional start offset.
        #[arg(long)]
        offset: Option<u64>,
        /// Optional byte count (required if `--offset` is set).
        #[arg(long)]
        size: Option<u64>,
    },

    /// Show the alternate (debug) IPv4 address.
    Altaddr,

    /// Show the running title's process id.
    Getpid,

    /// Show the console memory class.
    Consolemem,

    /// Show the debug and title network-address blobs.
    Netaddrs,

    /// List loaded kernel and user modules.
    Modules,

    /// List sections of a named module.
    Modsections { module: String },

    /// List live thread ids.
    Threads,

    /// Show detailed info for one thread id (hex, `0x...`).
    Threadinfo { thread: String },

    /// Show metadata for the currently running title (or a named xex).
    Xbeinfo {
        #[arg(long)]
        name: Option<String>,
    },

    /// Enumerate the console's virtual-address ranges.
    Walkmem,

    /// Read memory into a hex dump (and mark unmapped pages).
    Getmem {
        /// Starting virtual address (hex, `0x...`).
        address: String,
        /// Length in bytes (decimal or `0x...`).
        length: String,
    },

    /// List the kernel's performance counters.
    Pclist,

    /// Query a single performance counter by name.
    Querypc {
        name: String,
        #[arg(long, default_value_t = 1)]
        kind: u32,
    },

    /// List the sockets XBDM is currently tracking.
    Sockets,

    /// Report whether a thread id is halted, and why.
    Isstopped { thread: String },

    /// Reboot the console. Flags may be combined.
    Reboot {
        #[arg(long)]
        warm: bool,
        #[arg(long = "stop")]
        stop_on_start: bool,
        #[arg(long = "nodebug")]
        no_debug: bool,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        title: Option<String>,
    },

    /// Set (or clear via `--nopersist`) the default title.
    SetTitle {
        #[arg(long)]
        nopersist: bool,
        #[arg(conflicts_with = "nopersist")]
        name: Option<String>,
    },

    /// Write bytes to console memory. Accepts a hex string of data.
    Setmem {
        /// Address (hex, `0x...`).
        address: String,
        /// Hex byte string (e.g. `DEADBEEF`). Must be even-length.
        hex: String,
    },

    /// Set or clear an execution breakpoint.
    Bp {
        address: String,
        #[arg(long)]
        clear: bool,
    },

    /// Set or clear a data-access breakpoint (read / write / rw / exec).
    Databp {
        address: String,
        /// Size in bytes.
        size: u32,
        /// Access kind: `read`, `write`, `readwrite`, or `execute`.
        #[arg(long, default_value = "write")]
        kind: String,
        #[arg(long)]
        clear: bool,
    },

    /// Truncate or extend a file on the console.
    Fileeof {
        path: String,
        size: u64,
        #[arg(long)]
        create: bool,
    },

    /// Upload a local file to a console path.
    Put {
        /// Local source file.
        local: String,
        /// Remote destination path.
        remote: String,
    },

    /// Write a local file's contents to a byte range of an existing
    /// console file.
    Writeto {
        local: String,
        remote: String,
        offset: u64,
        length: u64,
    },

    /// Capture a screenshot and save it as PNG (or raw framebuffer when
    /// `--raw` is set).
    Screenshot {
        /// Destination path. `.png` for a PNG (default). Pass `--raw` to
        /// emit the raw framebuffer bytes instead.
        #[arg(short, long, default_value = "screenshot.png")]
        output: String,
        /// Write the raw framebuffer bytes + a `.meta` sidecar instead
        /// of encoding a PNG. Useful for formats we haven't decoded yet.
        #[arg(long)]
        raw: bool,
    },

    /// Drive the PIX movie-capture session on the console. Emits raw
    /// intermediate segments to the console's HDD, downloads them to
    /// the local `--output-dir`, and prints a notification log. Does
    /// not transcode to WMV/MP4 yet -- that's Phase 2.
    #[cfg(feature = "capture")]
    Capture {
        /// Device-side filename (without the
        /// `\Device\Harddisk0\Partition1\DEVKIT\` prefix that xbmovie
        /// traditionally adds); xeedee passes this through verbatim.
        #[arg(long, default_value = "DEVKIT:\\xeedee_capture.wmv")]
        remote: String,
        /// Per-segment size cap in megabytes. Default 512 matches xbmovie.
        #[arg(long, default_value_t = 512)]
        size_limit_mb: u32,
        /// Capture duration in seconds. If unset, capture continues
        /// until you press Enter.
        #[arg(long)]
        duration: Option<u64>,
        /// Local directory to drop downloaded segments into.
        #[arg(long, default_value = "./xeedee-capture")]
        output_dir: String,
    },

    /// Send a raw command line and print the parsed response.
    Raw {
        /// The command line to send (no trailing CRLF).
        line: String,
    },

    /// Dangerous, low-level operations that write into xbdm's memory
    /// image. Each subcommand is gated behind the `dangerous` feature
    /// and documented with what it touches.
    #[cfg(feature = "dangerous")]
    #[command(subcommand)]
    Dangerous(DangerousCommand),
}

#[cfg(feature = "dangerous")]
#[derive(Subcommand, Debug)]
enum DangerousCommand {
    /// Enable `drivemap internal=1` for this session by calling xbdm's
    /// own symlink-setup routine via a one-shot command-table swap. No
    /// flash writes; the effect lasts until reboot.
    DrivemapEnable,
    /// Report current state of the drivemap flag + visible drives.
    DrivemapStatus,
    /// Write `[xbdm]\r\ndrivemap internal=1\r\n` to `FLASH:\recint.ini`
    /// so the setting survives reboots. Requires `drivemap-enable` to
    /// have been run already so `FLASH:` is mounted.
    DrivemapPersist,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&cli.log)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(report) => {
            eprintln!("error: {report:?}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct UiCtx {
    format: OutputFormat,
    no_progress: bool,
    raw_bytes: bool,
}

impl UiCtx {
    fn pretty(&self) -> bool {
        self.format == OutputFormat::Pretty
    }
    fn progress(&self, total: u64, label: &str) -> ProgressBar {
        ui::transfer_bar(total, label, self.no_progress)
    }
    fn fmt_bytes(&self, n: u64) -> String {
        if self.raw_bytes {
            n.to_string()
        } else {
            humanize_bytes(n)
        }
    }
}

fn sort_dir_entries(
    entries: &mut [xeedee::commands::DirEntry],
    sort_by: LsSortBy,
    order: SortOrder,
) {
    use core::cmp::Ordering;
    entries.sort_by(|a, b| {
        let primary = match sort_by {
            LsSortBy::Name => a
                .name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase()),
            LsSortBy::Size => a.size.cmp(&b.size),
            LsSortBy::Kind => a.is_directory.cmp(&b.is_directory),
            LsSortBy::Modified => a.change_time.as_raw().cmp(&b.change_time.as_raw()),
            LsSortBy::Created => a.create_time.as_raw().cmp(&b.create_time.as_raw()),
        };
        let ordered = match order {
            SortOrder::Asc => primary,
            SortOrder::Desc => primary.reverse(),
        };
        if ordered != Ordering::Equal {
            return ordered;
        }
        // Stable tie-breaker: name ascending so adjacent equal keys stay readable.
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
}

fn humanize_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut idx = 0usize;
    while v >= 1024.0 && idx < UNITS.len() - 1 {
        v /= 1024.0;
        idx += 1;
    }
    format!("{v:.2} {}", UNITS[idx])
}

async fn run(cli: Cli) -> Result<(), rootcause::Report<Error>> {
    let ui = UiCtx {
        format: cli.format,
        no_progress: cli.no_progress,
        raw_bytes: cli.bytes,
    };

    match &cli.cmd {
        Command::Discover {
            listen_ms,
            broadcast,
        } => {
            return run_discover(*listen_ms, broadcast.as_deref(), ui).await;
        }
        Command::Resolve { name, listen_ms } => {
            return run_resolve(name, *listen_ms).await;
        }
        _ => {}
    }

    let host = cli.host.as_deref().ok_or_else(|| {
        rootcause::Report::new(Error::from(xeedee::error::ArgumentError::EmptyFilename))
            .attach("--host is required for this subcommand")
    })?;
    let target = Target::parse(host, cli.port);
    let conn_timeout = Duration::from_secs(cli.timeout);
    tracing::info!(target: "xeedee", console = %target, "connecting");

    // drivemap-enable needs two independent connections (the first one
    // will be abandoned with a hung read), so it bypasses the usual
    // single-client flow and is not recorded under --capture.
    #[cfg(feature = "dangerous")]
    if matches!(
        &cli.cmd,
        Command::Dangerous(DangerousCommand::DrivemapEnable)
    ) {
        return run_drivemap_enable(&target, conn_timeout).await;
    }

    let transport = connect_target_timeout(&target, conn_timeout).await?;

    if let Some(path) = cli.capture.clone() {
        let recording = RecordingTransport::new(transport);
        let log_handle = recording.log_handle();

        let outcome = drive(recording.into_client(), cli.cmd, ui).await;
        write_capture(&path, &log_handle)?;

        outcome
    } else {
        let client = Client::new(transport).read_banner().await?;
        drive_connected(client, cli.cmd, ui).await
    }
}

#[cfg(feature = "dangerous")]
async fn run_drivemap_enable(
    target: &Target,
    conn_timeout: Duration,
) -> Result<(), rootcause::Report<Error>> {
    let hijack_transport = connect_target_timeout(target, conn_timeout).await?;
    let hijack = Client::new(hijack_transport).read_banner().await?;

    let target_for_reconnect = target.clone();
    let reconnect = move || {
        let target = target_for_reconnect.clone();
        async move {
            let t = connect_target_timeout(&target, conn_timeout).await?;
            let c = Client::new(t).read_banner().await?;
            Ok::<_, rootcause::Report<Error>>(c)
        }
    };

    let report = dm::enable(hijack, reconnect, Duration::from_secs(3)).await?;
    if report.already_enabled {
        eprintln!(
            "{} drivemap flag already set; skipped hijack. {} drive{} visible.",
            ok_tag("no-op"),
            report.drives_after.len(),
            if report.drives_after.len() == 1 {
                ""
            } else {
                "s"
            }
        );
    } else {
        eprintln!(
            "{} drivemap internal enabled. drives went from {} -> {} entries.",
            ok_tag("done"),
            report.drives_before.len(),
            report.drives_after.len()
        );
    }
    for drive in &report.drives_after {
        println!("{drive}");
    }
    Ok(())
}

trait RecordingExt<T> {
    fn into_client(self) -> Client<RecordingTransport<T>>;
}

impl<T> RecordingExt<T> for RecordingTransport<T>
where
    T: futures_io::AsyncRead + futures_io::AsyncWrite + Unpin,
{
    fn into_client(self) -> Client<RecordingTransport<T>> {
        Client::new(self)
    }
}

async fn drive<T>(
    client: Client<T>,
    cmd: Command,
    ui: UiCtx,
) -> Result<(), rootcause::Report<Error>>
where
    T: futures_io::AsyncRead + futures_io::AsyncWrite + Unpin,
{
    let client = client.read_banner().await?;
    drive_connected(client, cmd, ui).await
}

async fn drive_connected<T>(
    mut client: Client<T, xeedee::Connected>,
    cmd: Command,
    ui: UiCtx,
) -> Result<(), rootcause::Report<Error>>
where
    T: futures_io::AsyncRead + futures_io::AsyncWrite + Unpin,
{
    match cmd {
        Command::Discover { .. } | Command::Resolve { .. } => unreachable!(),
        Command::Ping => {
            println!("connected");
        }
        Command::Dbgname { set } => {
            let name = client
                .run(match set {
                    Some(value) => DbgName::Set(value),
                    None => DbgName::Get,
                })
                .await?;
            println!("{name}");
        }
        Command::Systime => {
            let result = client.run(SysTime).await?;
            let sys = result.file_time.into_system_time();
            let unix = sys
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            println!(
                "filetime_raw={raw:#x} unix_seconds={unix}",
                raw = result.file_time.as_raw()
            );
        }
        Command::Dmversion => {
            let version = client.run(DmVersion).await?;
            println!("{version}");
        }
        Command::Consoletype => {
            let kind = client.run(GetConsoleType).await?;
            println!("{kind:?}");
        }
        Command::Consolefeatures => {
            let features = client.run(GetConsoleFeatures).await?;
            for flag in features.flags {
                println!("{flag}");
            }
        }
        Command::Drivelist => {
            let drives = client.run(DriveList).await?;
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row<'a> {
                    #[tabled(rename = "drive")]
                    name: &'a str,
                }
                let rows: Vec<Row<'_>> = drives.iter().map(|d| Row { name: d }).collect();
                print_colored_table(&heading_label("drives", &ui), rows, &ui);
            } else {
                for d in drives {
                    println!("{d}");
                }
            }
        }
        Command::Df { drive } => {
            let space = client
                .run(DriveFreeSpace {
                    drive: drive.clone(),
                })
                .await?;
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row {
                    #[tabled(rename = "metric")]
                    key: String,
                    size: String,
                }
                let rows = vec![
                    Row {
                        key: "free_to_caller".into(),
                        size: ui.fmt_bytes(space.free_to_caller_bytes),
                    },
                    Row {
                        key: "total".into(),
                        size: ui.fmt_bytes(space.total_bytes),
                    },
                    Row {
                        key: "total_free".into(),
                        size: ui.fmt_bytes(space.total_free_bytes),
                    },
                ];
                print_colored_table(&heading_label(&format!("df {drive}"), &ui), rows, &ui);
            } else {
                println!(
                    "free_to_caller={} total={} total_free={}",
                    ui.fmt_bytes(space.free_to_caller_bytes),
                    ui.fmt_bytes(space.total_bytes),
                    ui.fmt_bytes(space.total_free_bytes)
                );
            }
        }
        Command::Tree {
            path,
            max_depth,
            files,
        } => {
            run_tree(&mut client, path, max_depth, files, ui).await?;
        }
        Command::Ls {
            path,
            sort_by,
            sort_order,
        } => {
            let mut entries = client.run(DirList { path: path.clone() }).await?;
            sort_dir_entries(&mut entries, sort_by, sort_order);
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row {
                    name: String,
                    size: String,
                    kind: String,
                    #[tabled(rename = "changed")]
                    changed: String,
                }
                let rows: Vec<Row> = entries
                    .iter()
                    .map(|e| Row {
                        name: e.name.clone(),
                        size: if e.is_directory {
                            "-".into()
                        } else {
                            ui.fmt_bytes(e.size)
                        },
                        kind: if e.is_directory {
                            "DIR".into()
                        } else {
                            "FILE".into()
                        },
                        changed: format!("{:#018x}", e.change_time.as_raw()),
                    })
                    .collect();
                print_colored_table(&heading_label(&format!("ls {path}"), &ui), rows, &ui);
            } else {
                for entry in entries {
                    println!(
                        "{}\t{}\t{}\t{}",
                        entry.name,
                        if entry.is_directory {
                            "-".into()
                        } else {
                            ui.fmt_bytes(entry.size)
                        },
                        if entry.is_directory { "DIR" } else { "FILE" },
                        entry.change_time.as_raw()
                    );
                }
            }
        }
        Command::Stat { path } => {
            let attrs = client.run(GetFileAttributes { path }).await?;
            println!(
                "size={} create_filetime={:#x} change_filetime={:#x} is_directory={}",
                if attrs.is_directory {
                    "-".into()
                } else {
                    ui.fmt_bytes(attrs.size)
                },
                attrs.create_time.as_raw(),
                attrs.change_time.as_raw(),
                attrs.is_directory
            );
        }
        Command::Mkdir { path } => {
            client.run(MakeDirectory { path }).await?;
        }
        Command::Rm { path, dir } => {
            client
                .run(Delete {
                    path,
                    is_directory: dir,
                })
                .await?;
        }
        Command::Mv { from, to } => {
            client.run(Rename { from, to }).await?;
        }
        Command::Get {
            remote,
            output,
            offset,
            size,
        } => {
            let range = match (offset, size) {
                (Some(offset), Some(size)) => GetFileRange::Range { offset, size },
                (None, None) => GetFileRange::WholeFile,
                _ => {
                    return Err(rootcause::Report::new(Error::from(
                        xeedee::error::ArgumentError::EmptyFilename,
                    ))
                    .attach("--offset and --size must be provided together"));
                }
            };
            let download = client.get_file(&remote, range).await?;
            let total = download.total();
            let bar = ui.progress(total, "download");
            if output == "-" {
                let stdout = tokio::io::stdout();
                let compat = tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(stdout);
                let mut tracked = ProgressWrite::new(compat, bar.clone());
                let copied = download.copy_into(&mut tracked).await?;
                bar.finish_and_clear();
                eprintln!(
                    "{} {copied} bytes ({total} declared) to stdout",
                    ok_tag("downloaded")
                );
            } else {
                let file = tokio::fs::File::create(&output)
                    .await
                    .map_err(Error::from)
                    .into_report()
                    .attach_with(|| format!("creating local file {output}"))?;
                let compat = tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(file);
                let mut tracked = ProgressWrite::new(compat, bar.clone());
                let copied = download.copy_into(&mut tracked).await?;
                bar.finish_and_clear();
                eprintln!(
                    "{} {copied} bytes ({total} declared) to {output}",
                    ok_tag("downloaded")
                );
            }
        }
        Command::Altaddr => {
            let addr = client.run(AltAddr).await?;
            println!("{addr}");
        }
        Command::Getpid => {
            let pid = client.run(GetPid).await?;
            println!("{pid:#010x}");
        }
        Command::Consolemem => {
            let mem = client.run(GetConsoleMem).await?;
            println!("class={:#04x}", mem.class);
        }
        Command::Netaddrs => {
            let na = client.run(GetNetAddrs).await?;
            println!("name={}", na.name);
            println!("debug={}", hex_dump(&na.debug));
            println!("title={}", hex_dump(&na.title));
        }
        Command::Modules => {
            let mods = client.run(Modules).await?;
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row {
                    name: String,
                    base: String,
                    end: String,
                    size: String,
                    check: String,
                    #[tabled(rename = "dll")]
                    dll: &'static str,
                    osize: String,
                }
                let rows: Vec<Row> = mods
                    .iter()
                    .map(|m| Row {
                        name: m.name.clone(),
                        base: format!("{:#010x}", m.base),
                        end: format!("{:#010x}", m.base.wrapping_add(m.size)),
                        size: format!("{:#010x}", m.size),
                        check: format!("{:#010x}", m.checksum),
                        dll: if m.is_dll { "yes" } else { "no" },
                        osize: format!("{:#010x}", m.osize),
                    })
                    .collect();
                print_colored_table(&heading_label("modules", &ui), rows, &ui);
            } else {
                for m in mods {
                    println!(
                        "{}\t{:#010x}\t{:#010x}\t{:#010x}\t{:#010x}\t{}\t{:#010x}",
                        m.name,
                        m.base,
                        m.base.wrapping_add(m.size),
                        m.size,
                        m.checksum,
                        m.is_dll,
                        m.osize
                    );
                }
            }
        }
        Command::Modsections { module } => {
            let sections = client
                .run(ModuleSections {
                    module: module.clone(),
                })
                .await?;
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row {
                    name: String,
                    base: String,
                    size: String,
                    idx: u32,
                    flags: String,
                    #[tabled(rename = "rwxu")]
                    perms: String,
                }
                let rows: Vec<Row> = sections
                    .iter()
                    .map(|s| Row {
                        name: s.name.clone(),
                        base: format!("{:#010x}", s.base),
                        size: format!("{:#010x}", s.size),
                        idx: s.index,
                        flags: format!("{:#x}", s.flags.0),
                        perms: format!(
                            "{}{}{}{}",
                            if s.flags.readable() { 'r' } else { '-' },
                            if s.flags.writable() { 'w' } else { '-' },
                            if s.flags.executable() { 'x' } else { '-' },
                            if s.flags.uninitialized() { 'u' } else { '-' },
                        ),
                    })
                    .collect();
                print_colored_table(
                    &heading_label(&format!("modsections {module}"), &ui),
                    rows,
                    &ui,
                );
            } else {
                for s in sections {
                    println!(
                        "{}\t{:#010x}\t{:#010x}\t{}\t{:#x}\t{}{}{}{}",
                        s.name,
                        s.base,
                        s.size,
                        s.index,
                        s.flags.0,
                        if s.flags.readable() { 'r' } else { '-' },
                        if s.flags.writable() { 'w' } else { '-' },
                        if s.flags.executable() { 'x' } else { '-' },
                        if s.flags.uninitialized() { 'u' } else { '-' },
                    );
                }
            }
        }
        Command::Threads => {
            let tids = client.run(Threads).await?;
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row {
                    #[tabled(rename = "thread id")]
                    id: String,
                }
                let rows: Vec<Row> = tids.iter().map(|t| Row { id: format!("{t}") }).collect();
                print_colored_table(&heading_label("threads", &ui), rows, &ui);
            } else {
                for t in tids {
                    println!("{t}");
                }
            }
        }
        Command::Threadinfo { thread } => {
            let id = parse_thread_id(&thread)?;
            let info = client.run(ThreadInfo { thread: id }).await?;
            println!("{info:#?}");
        }
        Command::Xbeinfo { name } => {
            let cmd = match name {
                Some(path) => XbeInfo::Named(path),
                None => XbeInfo::Running,
            };
            let info = client.run(cmd).await?;
            println!("name={}", info.name);
            println!("timestamp={:#010x}", info.timestamp);
            println!("checksum={:#010x}", info.checksum);
        }
        Command::Walkmem => {
            let regions = client.run(WalkMem).await?;
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row {
                    base: String,
                    size: String,
                    protect: String,
                }
                let rows: Vec<Row> = regions
                    .iter()
                    .map(|r| Row {
                        base: format!("{:#010x}", r.base),
                        size: format!("{:#010x}", r.size),
                        protect: format!("{:#010x}", r.protect),
                    })
                    .collect();
                print_colored_table(&heading_label("walkmem", &ui), rows, &ui);
            } else {
                for region in regions {
                    println!(
                        "base={:#010x} size={:#010x} protect={:#010x}",
                        region.base, region.size, region.protect
                    );
                }
            }
        }
        Command::Getmem { address, length } => {
            let addr = parse_u32(&address).map_err(|e| attach_hint(e, "--address"))?;
            let len = parse_u32(&length).map_err(|e| attach_hint(e, "--length"))?;
            let snap = client
                .run(GetMem {
                    address: addr,
                    length: len,
                })
                .await?;
            print_hex_dump(snap.address, &snap.data, &snap.unmapped_offsets);
        }
        Command::Pclist => {
            let counters = client.run(PerfCounterList).await?;
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row {
                    #[tabled(rename = "type")]
                    kind: String,
                    name: String,
                }
                let rows: Vec<Row> = counters
                    .iter()
                    .map(|c| Row {
                        kind: format!("{:#010x}", c.kind),
                        name: c.name.clone(),
                    })
                    .collect();
                print_colored_table(&heading_label("pclist", &ui), rows, &ui);
            } else {
                for c in counters {
                    println!("{:#010x}\t{}", c.kind, c.name);
                }
            }
        }
        Command::Querypc { name, kind } => {
            let sample = client.run(QueryPerfCounter { name, kind }).await?;
            println!(
                "type={:#010x} value={} rate={}",
                sample.kind, sample.value, sample.rate
            );
        }
        Command::Sockets => {
            let sockets = client.run(GetSocketInfo).await?;
            if ui.pretty() {
                #[derive(Tabled)]
                struct Row {
                    handle: String,
                    af: u32,
                    #[tabled(rename = "type")]
                    sock_type: u32,
                    proto: u32,
                    local: String,
                    remote: String,
                    tcpstate: u32,
                    flags: String,
                }
                let rows: Vec<Row> = sockets
                    .iter()
                    .map(|s| Row {
                        handle: format!("{:#010x}", s.handle),
                        af: s.addr_family,
                        sock_type: s.socket_type,
                        proto: s.protocol,
                        local: format!("{:#010x}:{:#06x}", s.local_addr, s.local_port),
                        remote: format!("{:#010x}:{:#06x}", s.remote_addr, s.remote_port),
                        tcpstate: s.tcp_state,
                        flags: format!("{:#010x}", s.flags),
                    })
                    .collect();
                print_colored_table(&heading_label("sockets", &ui), rows, &ui);
            } else {
                for s in sockets {
                    println!(
                        "handle={:#010x} af={} type={} proto={} local={:#010x}:{:#06x} remote={:#010x}:{:#06x} tcpstate={} flags={:#010x}",
                        s.handle,
                        s.addr_family,
                        s.socket_type,
                        s.protocol,
                        s.local_addr,
                        s.local_port,
                        s.remote_addr,
                        s.remote_port,
                        s.tcp_state,
                        s.flags
                    );
                }
            }
        }
        Command::Isstopped { thread } => {
            let id = parse_thread_id(&thread)?;
            let state = client.run(IsStopped { thread: id }).await?;
            println!("{state:?}");
        }
        Command::Reboot {
            warm,
            stop_on_start,
            no_debug,
            wait,
            title,
        } => {
            client
                .run(Reboot {
                    flags: RebootFlags {
                        warm,
                        stop_on_start,
                        no_debug,
                        wait,
                    },
                    title,
                    directory: None,
                    cmd_line: None,
                })
                .await?;
            println!("reboot issued");
        }
        Command::SetTitle { nopersist, name } => {
            let cmd = if nopersist {
                Title::NoPersist
            } else {
                Title::Set {
                    name: name.unwrap_or_default(),
                }
            };
            client.run(cmd).await?;
        }
        Command::Setmem { address, hex } => {
            let addr = parse_u32(&address).map_err(rootcause::Report::new)?;
            let data = decode_hex(&hex)
                .map_err(rootcause::Report::new)
                .attach_with(|| format!("decoding --data {hex:?}"))?;
            let result = client
                .run(SetMem {
                    address: addr,
                    data,
                })
                .await?;
            println!("requested={} written={}", result.requested, result.written);
        }
        Command::Bp { address, clear } => {
            let addr = parse_u32(&address).map_err(rootcause::Report::new)?;
            client
                .run(Breakpoint {
                    address: addr,
                    clear,
                })
                .await?;
        }
        Command::Databp {
            address,
            size,
            kind,
            clear,
        } => {
            let addr = parse_u32(&address).map_err(rootcause::Report::new)?;
            let kind = match kind.to_ascii_lowercase().as_str() {
                "read" => DataBreakKind::Read,
                "write" => DataBreakKind::Write,
                "readwrite" | "rw" => DataBreakKind::ReadWrite,
                "execute" | "exec" => DataBreakKind::Execute,
                _ => {
                    return Err(rootcause::Report::new(Error::from(
                        xeedee::error::ArgumentError::EmptyFilename,
                    ))
                    .attach("kind must be one of read/write/readwrite/execute"));
                }
            };
            client
                .run(DataBreakpoint {
                    address: addr,
                    size,
                    kind,
                    clear,
                })
                .await?;
        }
        Command::Fileeof { path, size, create } => {
            client
                .run(FileEof {
                    path,
                    size,
                    create_if_missing: create,
                })
                .await?;
        }
        Command::Put { local, remote } => {
            let metadata = std::fs::metadata(&local)
                .map_err(Error::from)
                .into_report()
                .attach_with(|| format!("stat local file {local:?}"))?;
            let size = metadata.len();
            let mut file = tokio::fs::File::open(&local)
                .await
                .map_err(Error::from)
                .into_report()
                .attach_with(|| format!("opening local file {local:?}"))?;
            let upload = client
                .send_file(&remote, FileUploadKind::Create { size })
                .await?;
            let bar = ui.progress(size, "upload");
            let compat = tokio_util::compat::TokioAsyncReadCompatExt::compat(&mut file);
            let mut tracked = ProgressRead::new(compat, bar.clone());
            upload.copy_from(&mut tracked).await?;
            bar.finish_and_clear();
            eprintln!("{} {size} bytes to {remote}", ok_tag("uploaded"));
        }
        Command::Writeto {
            local,
            remote,
            offset,
            length,
        } => {
            let mut file = tokio::fs::File::open(&local)
                .await
                .map_err(Error::from)
                .into_report()
                .attach_with(|| format!("opening local file {local:?}"))?;
            let upload = client
                .send_file(
                    &remote,
                    FileUploadKind::WriteAt {
                        offset,
                        size: length,
                    },
                )
                .await?;
            let bar = ui.progress(length, "write");
            let compat = tokio_util::compat::TokioAsyncReadCompatExt::compat(&mut file);
            let mut tracked = ProgressRead::new(compat, bar.clone());
            upload.copy_from(&mut tracked).await?;
            bar.finish_and_clear();
            eprintln!(
                "{} {length} bytes at offset {offset} in {remote}",
                ok_tag("wrote")
            );
        }
        Command::Screenshot { output, raw } => {
            let shot = client.screenshot().await?;
            let meta = shot.metadata;
            eprintln!(
                "{} {}x{} pitch={} bytes={} format={:?}",
                ok_tag("captured"),
                meta.width,
                meta.height,
                meta.pitch,
                meta.framebuffer_size,
                meta.format
            );
            if raw {
                std::fs::write(&output, &shot.data)
                    .map_err(Error::from)
                    .into_report()
                    .attach_with(|| format!("writing raw framebuffer to {output}"))?;
                let sidecar = format!("{output}.meta");
                let meta_text = format!(
                    "pitch={}\nwidth={}\nheight={}\nformat={:?}\nframebuffer_size={}\npitch_hex={:#010x}\nformat_raw={:#010x}\n",
                    meta.pitch,
                    meta.width,
                    meta.height,
                    meta.format,
                    meta.framebuffer_size,
                    meta.pitch,
                    meta.format.raw()
                );
                std::fs::write(&sidecar, meta_text)
                    .map_err(Error::from)
                    .into_report()
                    .attach("writing metadata sidecar")?;
                eprintln!(
                    "{} raw framebuffer + metadata to {output} (+ {sidecar})",
                    ok_tag("wrote"),
                );
            } else {
                let rgba = shot.to_rgba8().ok_or_else(|| {
                    rootcause::Report::new(Error::from(
                        xeedee::error::ArgumentError::EmptyFilename,
                    ))
                    .attach(format!(
                        "cannot convert format {:?} to PNG without de-tiling; re-run with --raw",
                        meta.format
                    ))
                })?;
                let buffer =
                    image::RgbaImage::from_raw(meta.width, meta.height, rgba).ok_or_else(|| {
                        rootcause::Report::new(Error::from(
                            xeedee::error::ArgumentError::EmptyFilename,
                        ))
                        .attach("RgbaImage::from_raw rejected the buffer dimensions")
                    })?;
                let png_msg = format!("encoding PNG to {output}");
                buffer
                    .save_with_format(&output, image::ImageFormat::Png)
                    .map_err(|e| {
                        rootcause::Report::new(Error::from(std::io::Error::other(e.to_string())))
                            .attach(png_msg)
                    })?;
                eprintln!("{} PNG to {output}", ok_tag("wrote"));
            }
        }
        #[cfg(feature = "capture")]
        Command::Capture {
            remote,
            size_limit_mb,
            duration,
            output_dir,
        } => {
            run_capture(
                &mut client,
                &remote,
                size_limit_mb,
                duration.map(Duration::from_secs),
                &output_dir,
                &ui,
            )
            .await?;
        }
        Command::Raw { line } => {
            let resp = client.send_raw(&line).await?;
            println!("{resp:#?}");
        }
        #[cfg(feature = "dangerous")]
        Command::Dangerous(cmd) => match cmd {
            DangerousCommand::DrivemapStatus => {
                let status = dm::status(&mut client).await?;
                println!(
                    "xbdm @ {:#010x}, drivemap_fn @ {:#010x}, flag @ {:#010x} = {:#010x}, altaddr entry @ {:#010x}",
                    status.layout.module.base,
                    status.layout.drivemap_fn,
                    status.layout.flag_global,
                    status.flag_value,
                    status.layout.altaddr_entry.name_ptr_addr
                );
                println!("visible drives: {:?}", status.visible_drives);
            }
            DangerousCommand::DrivemapEnable => unreachable!(
                "DrivemapEnable is handled by run_drivemap_enable before drive_connected"
            ),
            DangerousCommand::DrivemapPersist => {
                let report = dm::persist(&mut client).await?;
                eprintln!(
                    "{} wrote {} bytes to {}",
                    ok_tag("persisted"),
                    report.bytes_written,
                    report.path
                );
            }
        },
    }

    let _ = client.bye().await;
    Ok(())
}

fn decode_hex(input: &str) -> Result<Vec<u8>, Error> {
    let trimmed = input.trim();
    if !trimmed.len().is_multiple_of(2) {
        return Err(Error::from(xeedee::ParseError::InvalidHexDigits {
            key: "hex-bytes",
        }));
    }
    let mut out = Vec::with_capacity(trimmed.len() / 2);
    for pair in trimmed.as_bytes().chunks(2) {
        let s = core::str::from_utf8(pair)
            .map_err(|_| Error::from(xeedee::ParseError::InvalidHexDigits { key: "hex-bytes" }))?;
        out.push(
            u8::from_str_radix(s, 16).map_err(|_| {
                Error::from(xeedee::ParseError::InvalidHexDigits { key: "hex-bytes" })
            })?,
        );
    }
    Ok(out)
}

fn parse_u32(input: &str) -> Result<u32, Error> {
    let trimmed = input.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
            .map_err(|_| Error::from(xeedee::ParseError::InvalidHexDigits { key: "cli" }))
    } else {
        trimmed
            .parse::<u32>()
            .map_err(|_| Error::from(xeedee::ParseError::InvalidDecimalU32 { key: "cli" }))
    }
}

fn parse_thread_id(input: &str) -> Result<ThreadId, rootcause::Report<Error>> {
    parse_u32(input)
        .map(ThreadId)
        .map_err(rootcause::Report::new)
        .attach_with(|| format!("invalid thread id {input:?}"))
}

fn attach_hint(err: Error, hint: &'static str) -> rootcause::Report<Error> {
    rootcause::Report::new(err).attach(hint)
}

fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

fn print_hex_dump(base: u32, data: &[u8], unmapped: &[u32]) {
    let unmapped: std::collections::HashSet<u32> = unmapped.iter().copied().collect();
    for (chunk_idx, chunk) in data.chunks(16).enumerate() {
        let addr = base.wrapping_add((chunk_idx * 16) as u32);
        print!("{addr:#010x}  ");
        for (i, b) in chunk.iter().enumerate() {
            let offset = (chunk_idx * 16 + i) as u32;
            if unmapped.contains(&offset) {
                print!("?? ");
            } else {
                print!("{:02x} ", b);
            }
            if i == 7 {
                print!(" ");
            }
        }
        for _ in chunk.len()..16 {
            print!("   ");
        }
        print!(" |");
        for (i, b) in chunk.iter().enumerate() {
            let offset = (chunk_idx * 16 + i) as u32;
            if unmapped.contains(&offset) || !(0x20..0x7f).contains(b) {
                print!(".");
            } else {
                print!("{}", *b as char);
            }
        }
        println!("|");
    }
}

#[cfg(feature = "capture")]
async fn run_capture<T>(
    client: &mut xeedee::Client<T, xeedee::Connected>,
    remote: &str,
    size_limit_mb: u32,
    duration: Option<Duration>,
    output_dir: &str,
    ui: &UiCtx,
) -> Result<(), rootcause::Report<Error>>
where
    T: futures_io::AsyncRead + futures_io::AsyncWrite + Unpin,
{
    std::fs::create_dir_all(output_dir)
        .map_err(Error::from)
        .into_report()
        .attach_with(|| format!("creating local output dir {output_dir}"))?;

    let mut session = CaptureSession::connect(client).await?;
    eprintln!("{} pix session ready", ok_tag("connected"));

    session.limit_capture_size_mb(size_limit_mb).await?;
    session.begin_capture_file_creation(remote).await?;
    eprintln!("{} prepared capture -> {remote}", ok_tag("ready"));

    session.begin_capture().await?;
    eprintln!(
        "{} capturing; {}",
        ok_tag("recording"),
        match duration {
            Some(d) => format!("stopping in {}s", d.as_secs()),
            None => "press Enter to stop".to_owned(),
        }
    );

    let stop_signal = async {
        match duration {
            Some(d) => {
                tokio::time::sleep(d).await;
            }
            None => {
                let stdin = tokio::io::BufReader::new(tokio::io::stdin());
                use tokio::io::AsyncBufReadExt as _;
                let mut lines = stdin.lines();
                let _ = lines.next_line().await;
            }
        }
    };
    stop_signal.await;

    session.end_capture().await?;
    session.end_capture_file_creation().await?;
    session.disconnect().await?;
    eprintln!(
        "{} capture wrapped up, downloading segments",
        ok_tag("done")
    );

    // Poll the console for segments (xbmovie uses notifications; polling
    // is fine for a v0 implementation). Segment file names follow the
    // pattern `<base>N.<ext>`: we strip the final `.wmv` on the remote
    // path and append `N.wmv` starting at 0.
    let (base, ext) = match remote.rfind('.') {
        Some(dot) => (&remote[..dot], &remote[dot..]),
        None => (remote, ""),
    };

    let mut segments_downloaded: u32 = 0;
    for index in 0u32..256 {
        let segment_remote = format!("{base}{index}{ext}");
        let attrs = client
            .run(GetFileAttributes {
                path: segment_remote.clone(),
            })
            .await;
        let attrs = match attrs {
            Ok(a) => a,
            Err(_) => break,
        };
        if attrs.is_directory {
            break;
        }
        let local_path = format!("{output_dir}/segment-{:03}.bin", index);
        let download = client
            .get_file(&segment_remote, GetFileRange::WholeFile)
            .await?;
        let total = download.total();
        let bar = ui.progress(total, &format!("segment {index}"));
        let file = tokio::fs::File::create(&local_path)
            .await
            .map_err(Error::from)
            .into_report()
            .attach_with(|| format!("creating {local_path}"))?;
        let compat = tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(file);
        let mut tracked = ProgressWrite::new(compat, bar.clone());
        download.copy_into(&mut tracked).await?;
        bar.finish_and_clear();
        eprintln!(
            "{} segment {index}: {total} bytes -> {local_path}",
            ok_tag("saved")
        );
        let _ = client
            .run(xeedee::commands::Delete {
                path: segment_remote,
                is_directory: false,
            })
            .await;
        segments_downloaded += 1;
    }

    eprintln!(
        "{} downloaded {segments_downloaded} segment{s}",
        ok_tag("finished"),
        s = if segments_downloaded == 1 { "" } else { "s" },
    );
    let _ = Notification::parse;
    Ok(())
}

async fn run_discover(
    listen_ms: u64,
    broadcast: Option<&str>,
    ui: UiCtx,
) -> Result<(), rootcause::Report<Error>> {
    let mut config = DiscoveryConfig::broadcast();
    config.listen_for = Duration::from_millis(listen_ms);
    if let Some(raw) = broadcast {
        let parsed: SocketAddr = raw
            .parse()
            .or_else(|_| format!("{raw}:{NAP_PORT}").parse::<SocketAddr>())
            .map_err(|_| {
                rootcause::Report::new(Error::from(
                    xeedee::error::ArgumentError::QuotedContainsCrlf,
                ))
                .attach(format!("invalid --broadcast value {raw:?}"))
            })?;
        config.destination = parsed;
    }
    tracing::info!(destination = %config.destination, "broadcasting nap identify");
    let results = discover_all(config).await?;
    if results.is_empty() {
        eprintln!("{} no consoles responded", warn_tag("warning:"));
        return Ok(());
    }
    if ui.pretty() {
        #[derive(Tabled)]
        struct Row {
            name: String,
            address: String,
        }
        let rows: Vec<Row> = results
            .iter()
            .map(|c| Row {
                name: c.name.clone(),
                address: c.addr.to_string(),
            })
            .collect();
        print_colored_table(&heading_label("discover", &ui), rows, &ui);
    } else {
        for console in results {
            println!("{}\t{}", console.name, console.addr);
        }
    }
    Ok(())
}

async fn run_resolve(name: &str, listen_ms: u64) -> Result<(), rootcause::Report<Error>> {
    let mut config = DiscoveryConfig::broadcast();
    config.listen_for = Duration::from_millis(listen_ms);
    config.destination = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), NAP_PORT);
    match find_by_name(name, config).await? {
        Some(console) => {
            println!("{}\t{}", console.name, console.addr);
            Ok(())
        }
        None => Err(rootcause::Report::new(Error::from(
            xeedee::error::TransportError::ConnectTimeout,
        ))
        .attach(format!("no reply from console named {name:?}"))),
    }
}

fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// Emit a line of output alongside an active progress bar/spinner
/// without the two clobbering each other. When a bar is present we
/// delegate to `ProgressBar::println`, which redraws itself after each
/// printed line; otherwise we fall through to plain `println!`.
fn tree_println(bar: Option<&ProgressBar>, line: impl AsRef<str>) {
    let line = line.as_ref();
    match bar {
        Some(bar) if !bar.is_hidden() => bar.println(line),
        _ => println!("{line}"),
    }
}

fn tree_dir_style(name: &str, is_dir: bool) -> String {
    if !colors_enabled() || !is_dir {
        return name.to_owned();
    }
    format!("{}", name.blue().bold())
}

async fn run_tree<T>(
    client: &mut Client<T, xeedee::Connected>,
    path: Option<String>,
    max_depth: u32,
    include_files: bool,
    ui: UiCtx,
) -> Result<(), rootcause::Report<Error>>
where
    T: futures_io::AsyncRead + futures_io::AsyncWrite + Unpin,
{
    let spinner = if !ui.no_progress && std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        let bar = ProgressBar::new_spinner();
        bar.set_draw_target(indicatif::ProgressDrawTarget::stderr());
        bar.set_style(
            indicatif::ProgressStyle::with_template("{spinner:.cyan} {prefix:.bold} dirs={msg}")
                .expect("valid spinner template"),
        );
        bar.enable_steady_tick(Duration::from_millis(120));
        bar.set_prefix("walking");
        Some(bar)
    } else {
        None
    };

    let roots: Vec<String> = match path {
        Some(p) => vec![normalize_dir_path(&p)],
        None => client
            .run(DriveList)
            .await?
            .into_iter()
            .map(|d| format!("{d}:\\"))
            .collect(),
    };

    let mut dir_count: u64 = 0;
    for root in &roots {
        walk(
            client,
            root,
            0,
            max_depth,
            include_files,
            ui,
            spinner.as_ref(),
            &mut dir_count,
            &[],
            true,
        )
        .await?;
    }

    if let Some(bar) = spinner {
        bar.finish_and_clear();
    }
    eprintln!(
        "{} {} dir{}",
        ok_tag("walked"),
        dir_count,
        if dir_count == 1 { "" } else { "s" }
    );
    Ok(())
}

fn normalize_dir_path(input: &str) -> String {
    if input.ends_with('\\') || input.is_empty() {
        input.to_owned()
    } else {
        format!("{input}\\")
    }
}

fn join_path(parent: &str, child: &str) -> String {
    if parent.ends_with('\\') {
        format!("{parent}{child}")
    } else {
        format!("{parent}\\{child}")
    }
}

async fn walk<T>(
    client: &mut Client<T, xeedee::Connected>,
    dir: &str,
    depth: u32,
    max_depth: u32,
    include_files: bool,
    ui: UiCtx,
    spinner: Option<&ProgressBar>,
    dir_count: &mut u64,
    ancestor_is_last: &[bool],
    is_root: bool,
) -> Result<(), rootcause::Report<Error>>
where
    T: futures_io::AsyncRead + futures_io::AsyncWrite + Unpin,
{
    *dir_count += 1;
    if let Some(bar) = spinner {
        bar.inc(1);
        bar.set_message(format!("{}", *dir_count));
    }
    if is_root {
        tree_println(spinner, tree_dir_style(dir.trim_end_matches('\\'), true));
    }

    let entries = match client
        .run(DirList {
            path: dir.to_owned(),
        })
        .await
    {
        Ok(entries) => entries,
        Err(err) => {
            let indent = tree_indent(ancestor_is_last, ui);
            let reason = format!("{:?}", err.current_context());
            let line = format!(
                "{indent}(unreadable) {}",
                if colors_enabled() {
                    format!("{}", reason.red())
                } else {
                    reason
                }
            );
            tree_println(spinner, line);
            return Ok(());
        }
    };

    let mut filtered: Vec<_> = entries
        .into_iter()
        .filter(|e| include_files || e.is_directory)
        .collect();
    filtered.sort_by(|a, b| {
        b.is_directory.cmp(&a.is_directory).then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        })
    });

    for (idx, entry) in filtered.iter().enumerate() {
        let last = idx == filtered.len() - 1;
        let indent = tree_indent(ancestor_is_last, ui);
        let branch = if ui.pretty() {
            if last { "└── " } else { "├── " }
        } else if last {
            "`-- "
        } else {
            "|-- "
        };
        let label = tree_dir_style(&entry.name, entry.is_directory);
        let line = if entry.is_directory {
            format!("{indent}{branch}{label}")
        } else {
            let size_text = format!("({})", ui.fmt_bytes(entry.size));
            let size_tag = if colors_enabled() {
                format!("  {}", size_text.dimmed())
            } else {
                format!("  {size_text}")
            };
            format!("{indent}{branch}{label}{size_tag}")
        };
        tree_println(spinner, line);

        if entry.is_directory && (max_depth == 0 || depth + 1 < max_depth) {
            let mut next_ancestors = ancestor_is_last.to_vec();
            next_ancestors.push(last);
            let child_dir = join_path(dir, &entry.name);
            let child_dir = normalize_dir_path(&child_dir);
            Box::pin(walk(
                client,
                &child_dir,
                depth + 1,
                max_depth,
                include_files,
                ui,
                spinner,
                dir_count,
                &next_ancestors,
                false,
            ))
            .await?;
        }
    }

    let _ = depth;
    Ok(())
}

fn tree_indent(ancestor_is_last: &[bool], ui: UiCtx) -> String {
    let mut out = String::new();
    for &last in ancestor_is_last {
        if ui.pretty() {
            out.push_str(if last { "    " } else { "│   " });
        } else {
            out.push_str(if last { "    " } else { "|   " });
        }
    }
    out
}

fn heading_label(label: &str, ui: &UiCtx) -> String {
    if ui.pretty() && colors_enabled() {
        format!("{}", label.cyan().bold())
    } else {
        label.to_owned()
    }
}

fn ok_tag(label: &str) -> String {
    if colors_enabled() {
        format!("{}", label.green().bold())
    } else {
        label.to_owned()
    }
}

fn warn_tag(label: &str) -> String {
    if colors_enabled() {
        format!("{}", label.yellow().bold())
    } else {
        label.to_owned()
    }
}

fn print_colored_table<I, T>(heading: &str, rows: I, ui: &UiCtx)
where
    I: IntoIterator<Item = T>,
    T: Tabled,
{
    if !heading.is_empty() {
        println!("{heading}");
    }
    let _ = ui;
    println!("{}", ui::styled_table(rows));
}

use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use futures_io::AsyncRead;
use futures_io::AsyncWrite;

/// Wrap an `AsyncWrite`, ticking an [`indicatif::ProgressBar`] as bytes
/// flow through.
struct ProgressWrite<W> {
    inner: W,
    bar: ProgressBar,
}

impl<W> ProgressWrite<W> {
    fn new(inner: W, bar: ProgressBar) -> Self {
        Self { inner, bar }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ProgressWrite<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => {
                this.bar.inc(n as u64);
                Poll::Ready(Ok(n))
            }
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// Wrap an `AsyncRead`, ticking an [`indicatif::ProgressBar`] as bytes
/// flow out.
struct ProgressRead<R> {
    inner: R,
    bar: ProgressBar,
}

impl<R> ProgressRead<R> {
    fn new(inner: R, bar: ProgressBar) -> Self {
        Self { inner, bar }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ProgressRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => {
                this.bar.inc(n as u64);
                Poll::Ready(Ok(n))
            }
        }
    }
}

fn write_capture(
    path: &PathBuf,
    handle: &Arc<Mutex<CaptureLog>>,
) -> Result<(), rootcause::Report<Error>> {
    let snapshot = handle.lock().expect("capture log poisoned").clone();
    std::fs::write(path, snapshot.to_text())
        .map_err(Error::from)
        .into_report()
        .attach_with(|| format!("writing capture to {}", path.display()))?;
    eprintln!("capture written to {}", path.display());
    Ok(())
}
