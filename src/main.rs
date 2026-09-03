use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const FLASH_BASE: u32 = 0x0040_0000;
const SRAM_BASE: u32 = 0x2000_0000;
const CHIPID_CIDR: u32 = 0x400e_0740;
const EEFC_FCR: u32 = 0x400e_0a04;
const EEFC_FRR: u32 = 0x400e_0a0c;
const SAM4E8C_EXID: u32 = 0x0012_0209;
const RPC_EOM: u8 = 0x1a;
const MARK: &str = "---SAM4E-OUT---";

#[derive(Parser)]
#[command(
    name = "sam4e",
    version,
    about = "Control an ATSAM4E8C through an Atmel-ICE",
    arg_required_else_help = true
)]
struct Cli {
    #[arg(short = 't', long, global = true, value_enum, default_value_t = Transport::Swd)]
    transport: Transport,

    #[arg(short = 's', long, global = true, default_value_t = 400, value_parser = clap::value_parser!(u32).range(1..))]
    speed: u32,

    #[arg(long, global = true, help = "Atmel-ICE serial number")]
    serial: Option<String>,

    #[arg(
        long,
        global = true,
        default_value = "openocd",
        help = "OpenOCD executable"
    )]
    openocd: PathBuf,

    #[arg(long, global = true, help = "Image configuration file")]
    config: Option<PathBuf>,

    #[arg(short = 'v', long, global = true, help = "Show the OpenOCD log")]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, ValueEnum)]
enum Transport {
    Swd,
    Jtag,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Swd => "swd",
            Self::Jtag => "jtag",
        })
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Show chip, memory, GPNVM, core, and link information
    Info,
    /// List named images
    Images,
    /// Program a named image or image file
    Flash {
        target: String,
        #[arg(short = 'a', long, help = "Required load address for a raw .bin")]
        addr: Option<String>,
        #[arg(long, action = ArgAction::SetTrue, overrides_with = "no_verify", help = "Verify after programming (default)")]
        verify: bool,
        #[arg(long = "no-verify", action = ArgAction::SetTrue, overrides_with = "verify", help = "Do not verify after programming")]
        no_verify: bool,
        #[arg(long, action = ArgAction::SetTrue, overrides_with = "no_run", help = "Reset and run after programming (default)")]
        run: bool,
        #[arg(long = "no-run", action = ArgAction::SetTrue, overrides_with = "run", help = "Do not reset after programming")]
        no_run: bool,
    },
    /// Select Flash or ROM (SAM-BA) boot
    Boot {
        mode: String,
        #[arg(long, action = ArgAction::SetTrue, overrides_with = "no_reset", help = "Reset after the change (default)")]
        reset: bool,
        #[arg(long = "no-reset", action = ArgAction::SetTrue, overrides_with = "reset", help = "Do not reset after the change")]
        no_reset: bool,
    },
    /// Read or change GPNVM bits
    Gpnvm {
        #[command(subcommand)]
        command: GpnvmCommand,
    },
    /// Erase all flash or an address range
    Erase {
        #[arg(long)]
        start: Option<String>,
        #[arg(long, help = "Exclusive end address")]
        end: Option<String>,
        #[arg(short = 'y', long, help = "Do not ask for confirmation")]
        yes: bool,
    },
    /// Read memory to the terminal or a file
    Read {
        #[arg(help = "Absolute start address")]
        addr: String,
        length: String,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Reset the target
    Reset {
        #[arg(long, help = "Stop at the reset vector")]
        halt: bool,
    },
    /// Halt the core
    Halt,
    /// Resume the core
    Resume,
    /// Run an OpenOCD GDB server until interrupted
    Gdb {
        #[arg(short = 'p', long, default_value_t = 3333, value_parser = parse_port)]
        port: u16,
    },
    /// Advanced: run raw OpenOCD Tcl without sam4e safety checks
    Raw {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
enum GpnvmCommand {
    /// Show GPNVM bits
    Show,
    /// Set a GPNVM bit to 1
    Set {
        bit: u8,
        #[arg(long, help = "Required when setting security bit 0")]
        force: bool,
    },
    /// Clear a GPNVM bit to 0
    Clear { bit: u8 },
}

enum AppError {
    Usage(String),
    Runtime(String),
    Exit(i32),
}

type AppResult<T> = Result<T, AppError>;

impl AppError {
    fn code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Runtime(_) => 1,
            Self::Exit(code) => *code,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) | Self::Runtime(message) => f.write_str(message),
            Self::Exit(_) => Ok(()),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    images: BTreeMap<String, Image>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    description: Option<String>,
    parts: Vec<ImagePart>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImagePart {
    file: PathBuf,
    addr: Option<u64>,
}

struct LoadedConfig {
    path: Option<PathBuf>,
    images: BTreeMap<String, Image>,
}

#[derive(Clone, Copy, PartialEq)]
enum ImageFormat {
    Bin,
    SelfAddressed,
}

struct FlashPart {
    file: PathBuf,
    addr: Option<u32>,
    format: ImageFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FlashRange {
    start: u32,
    end: u32,
}

enum FlashPlan {
    Single,
    MultiBin(Vec<FlashRange>),
}

struct OpenOcdOptions {
    executable: PathBuf,
    transport: Transport,
    speed: u32,
    serial: Option<String>,
    verbose: bool,
}

struct SessionError {
    message: String,
    retryable: bool,
}

impl SessionError {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }

    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
}

struct Session {
    child: Child,
    socket: Option<TcpStream>,
    log: Arc<Mutex<VecDeque<String>>>,
}

#[derive(Debug, PartialEq, Eq)]
struct FlashDescriptor {
    id: u32,
    size: u32,
    page_size: u32,
    planes: u32,
    lock_regions: u32,
    lock_size: u32,
}

struct ChipInfo {
    cidr: u32,
    exid: u32,
    flash: FlashDescriptor,
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = execute(&cli) {
        if !matches!(error, AppError::Exit(_)) {
            eprintln!("error: {error}");
        }
        std::process::exit(error.code());
    }
}

fn execute(cli: &Cli) -> AppResult<()> {
    validate_tcl_value(cli.serial.as_deref(), "serial number")?;
    let options = OpenOcdOptions {
        executable: cli.openocd.clone(),
        transport: cli.transport,
        speed: cli.speed,
        serial: cli.serial.clone(),
        verbose: cli.verbose,
    };

    match &cli.command {
        Commands::Info => command_info(&options),
        Commands::Images => command_images(cli),
        Commands::Flash {
            target,
            addr,
            no_verify,
            no_run,
            ..
        } => command_flash(cli, &options, target, addr.as_deref(), !no_verify, !no_run),
        Commands::Boot { mode, no_reset, .. } => command_boot(&options, mode, !no_reset),
        Commands::Gpnvm { command } => command_gpnvm(&options, command),
        Commands::Erase { start, end, yes } => {
            command_erase(&options, start.as_deref(), end.as_deref(), *yes)
        }
        Commands::Read { addr, length, out } => {
            command_read(&options, addr, length, out.as_deref())
        }
        Commands::Reset { halt } => command_reset(&options, *halt),
        Commands::Halt => command_halt(&options),
        Commands::Resume => command_resume(&options),
        Commands::Gdb { port } => command_gdb(&options, *port),
        Commands::Raw { command } => command_raw(&options, command),
    }
}

fn command_info(options: &OpenOcdOptions) -> AppResult<()> {
    let mut session = connect(options)?;
    let chip = ChipInfo::read(&mut session)?;
    let bits = read_gpnvm(&mut session)?;
    let state = session.value("$_TARGETNAME curstate")?;

    println!("SAM4E");
    println!("  Part          {}", chip.name());
    println!(
        "  CHIPID        CIDR={:#010x} EXID={:#010x}",
        chip.cidr, chip.exid
    );
    println!(
        "  Flash         {} KB, {:#010x}-{:#010x} ({} pages x {} B)",
        chip.flash.size / 1024,
        FLASH_BASE,
        chip.flash_end() - 1,
        chip.pages(),
        chip.flash.page_size
    );
    println!(
        "  Lock regions  {} x {} KB",
        chip.flash.lock_regions,
        chip.flash.lock_size / 1024
    );
    println!("  SRAM          {} KB at {SRAM_BASE:#010x}", chip.sram_kb());
    println!("  Core          Cortex-M4, {state}");
    println!(
        "  Link          {} @ {} kHz via Atmel-ICE",
        options.transport.to_string().to_uppercase(),
        options.speed
    );
    if chip.cidr_flash_kb() != chip.flash.size / 1024 {
        println!(
            "note: CHIPID reports {} KB of flash, but EEFC reports {} KB; using EEFC",
            chip.cidr_flash_kb(),
            chip.flash.size / 1024
        );
    }
    print_gpnvm(bits);
    Ok(())
}

fn command_images(cli: &Cli) -> AppResult<()> {
    let config = load_config(cli.config.as_deref())?;
    match &config.path {
        Some(path) => println!("Config: {}", path.display()),
        None => println!("Config: none"),
    }
    if config.images.is_empty() {
        println!("No images are defined.");
        return Ok(());
    }
    for (name, image) in config.images {
        println!(
            "\n{name}{}",
            image
                .description
                .map(|d| format!(" - {d}"))
                .unwrap_or_default()
        );
        for part in image.parts {
            let location = match part.addr {
                Some(address) => format!("{:#010x}", normalize_flash_addr(address)?),
                None => "in image".to_owned(),
            };
            let missing = if part.file.is_file() {
                ""
            } else {
                " (missing)"
            };
            println!("  {location:>10}  {}{missing}", part.file.display());
        }
    }
    Ok(())
}

fn command_flash(
    cli: &Cli,
    options: &OpenOcdOptions,
    target: &str,
    addr: Option<&str>,
    verify: bool,
    run_after: bool,
) -> AppResult<()> {
    let config = load_config(cli.config.as_deref())?;
    let parts = prepare_flash_parts(target, addr, &config)?;
    let mut session = connect(options)?;
    let chip = ChipInfo::read(&mut session)?;
    chip.require_sam4e8c()?;
    let plan = plan_flash(&parts, chip.flash_end())?;

    match plan {
        FlashPlan::Single => {
            write_part(&mut session, &parts[0], true)?;
            if verify {
                verify_part(&mut session, &parts[0])?;
            }
        }
        FlashPlan::MultiBin(ranges) => {
            for range in ranges {
                println!("Erasing {:#010x}-{:#010x}...", range.start, range.end - 1);
                session.run(&format!(
                    "flash erase_address unlock {:#x} {:#x}",
                    range.start,
                    range.end - range.start
                ))?;
            }
            for part in &parts {
                write_part(&mut session, part, false)?;
            }
            if verify {
                for part in &parts {
                    verify_part(&mut session, part)?;
                }
            }
        }
    }
    if run_after {
        session.run("reset run")?;
        println!("ok: reset, running");
    }
    Ok(())
}

fn command_boot(options: &OpenOcdOptions, mode: &str, reset_now: bool) -> AppResult<()> {
    let wanted = match mode.to_ascii_lowercase().as_str() {
        "flash" | "app" => 1,
        "samba" | "sam-ba" | "rom" | "bootloader" => 0,
        _ => return Err(AppError::Usage("mode must be 'flash' or 'samba'".into())),
    };
    let mut session = connect(options)?;
    require_sam4e8c(&mut session)?;
    let before = read_gpnvm(&mut session)?;
    if before[1] == wanted {
        println!(
            "ok: already booting from {}",
            if wanted == 1 { "Flash" } else { "ROM (SAM-BA)" }
        );
    } else {
        session.run(&format!(
            "at91sam4 gpnvm {} 1",
            if wanted == 1 { "set" } else { "clear" }
        ))?;
        thread::sleep(Duration::from_millis(50));
        if read_gpnvm(&mut session)?[1] != wanted {
            return Err(AppError::Runtime("GPNVM1 did not change".into()));
        }
        println!(
            "ok: boot source -> {}",
            if wanted == 1 {
                "Flash (application)"
            } else {
                "ROM (SAM-BA bootloader)"
            }
        );
    }
    if reset_now {
        session.run("reset run")?;
        println!("ok: device reset");
    }
    Ok(())
}

fn command_gpnvm(options: &OpenOcdOptions, command: &GpnvmCommand) -> AppResult<()> {
    match command {
        GpnvmCommand::Show => {
            let mut session = connect(options)?;
            require_sam4e8c(&mut session)?;
            print_gpnvm(read_gpnvm(&mut session)?);
            Ok(())
        }
        GpnvmCommand::Set { bit, force } => gpnvm_write(options, *bit, 1, *force),
        GpnvmCommand::Clear { bit } => gpnvm_write(options, *bit, 0, false),
    }
}

fn gpnvm_write(options: &OpenOcdOptions, bit: u8, value: u8, force: bool) -> AppResult<()> {
    if bit > 1 {
        return Err(AppError::Usage(format!(
            "GPNVM bit {bit} is not defined on the ATSAM4E8C (valid: 0, 1)"
        )));
    }
    if bit == 0 && value == 1 && !force {
        return Err(AppError::Usage(
            "setting GPNVM bit 0 disables JTAG and SWD; the ERASE pin restores access and erases all flash; use --force to continue".into(),
        ));
    }

    let mut session = connect(options)?;
    require_sam4e8c(&mut session)?;
    let before = read_gpnvm(&mut session)?;
    session.run(&format!(
        "at91sam4 gpnvm {} {bit}",
        if value == 1 { "set" } else { "clear" }
    ))?;
    thread::sleep(Duration::from_millis(50));
    let after = read_gpnvm(&mut session)?;
    if after[bit as usize] != value {
        return Err(AppError::Runtime(format!(
            "GPNVM{bit} is still {}, expected {value}",
            after[bit as usize]
        )));
    }
    println!(
        "ok: GPNVM{bit} ({}): {} -> {}",
        if bit == 0 { "security" } else { "boot_mode" },
        before[bit as usize],
        after[bit as usize]
    );
    print_gpnvm(after);
    if bit == 1 {
        println!("The change takes effect after reset. Run: sam4e reset");
    }
    Ok(())
}

fn command_erase(
    options: &OpenOcdOptions,
    start: Option<&str>,
    end: Option<&str>,
    yes: bool,
) -> AppResult<()> {
    let parsed_start = start.map(parse_flash_addr).transpose()?;
    let parsed_end = end.map(parse_flash_addr).transpose()?;
    let mut session = connect(options)?;
    let chip = ChipInfo::read(&mut session)?;
    chip.require_sam4e8c()?;
    let whole = parsed_start.is_none() && parsed_end.is_none();
    let a = parsed_start.unwrap_or(FLASH_BASE);
    let b = parsed_end.unwrap_or(chip.flash_end());
    if !whole && !(FLASH_BASE <= a && a < b && b <= chip.flash_end()) {
        return Err(AppError::Usage(format!(
            "{a:#010x}-{b:#010x} is outside flash ({:#010x}-{:#010x})",
            FLASH_BASE,
            chip.flash_end()
        )));
    }
    let what = if whole {
        format!("the entire {} KB flash", chip.flash.size / 1024)
    } else {
        format!("flash {a:#010x}-{:#010x} ({} KB)", b - 1, (b - a) / 1024)
    };
    if !yes && !confirm(&format!("Erase {what}?"))? {
        return Err(AppError::Exit(1));
    }
    println!("Erasing {what}...");
    if whole {
        session.run("flash erase_sector 0 0 last")?;
    } else {
        session.run(&format!("flash erase_address {a:#x} {:#x}", b - a))?;
    }
    println!("ok: erased {what}");
    Ok(())
}

fn command_read(
    options: &OpenOcdOptions,
    addr: &str,
    length: &str,
    out: Option<&Path>,
) -> AppResult<()> {
    let address = parse_absolute_addr(addr)?;
    let length = parse_positive_usize(length, "length")?;
    let output = out.map(absolute_path).transpose()?;
    if let Some(path) = &output {
        path_string(path)?;
    }
    let mut session = connect(options)?;
    if let Some(path) = output {
        println!("Reading {length} bytes from {address:#010x}...");
        session.run(&format!(
            "dump_image {} {address:#x} {length:#x}",
            tcl_literal(&path_string(&path)?)
        ))?;
        println!("ok: wrote {length} bytes to {}", path.display());
        return Ok(());
    }

    let words = session.read32(address, length.saturating_add(3) / 4)?;
    let mut data = Vec::with_capacity(words.len() * 4);
    for word in words {
        data.extend_from_slice(&word.to_le_bytes());
    }
    data.truncate(length);
    for (line, bytes) in data.chunks(16).enumerate() {
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let text: String = bytes
            .iter()
            .map(|byte| {
                if (32..127).contains(byte) {
                    char::from(*byte)
                } else {
                    '.'
                }
            })
            .collect();
        println!(
            "{:08x}  {:<47}  |{}|",
            address as usize + line * 16,
            hex,
            text
        );
    }
    Ok(())
}

fn command_reset(options: &OpenOcdOptions, halt: bool) -> AppResult<()> {
    let mut session = connect(options)?;
    session.run(if halt { "reset halt" } else { "reset run" })?;
    println!("ok: reset ({})", if halt { "halted" } else { "running" });
    Ok(())
}

fn command_halt(options: &OpenOcdOptions) -> AppResult<()> {
    let mut session = connect(options)?;
    println!("ok: core {}", session.value("$_TARGETNAME curstate")?);
    Ok(())
}

fn command_resume(options: &OpenOcdOptions) -> AppResult<()> {
    let mut session = connect(options)?;
    session.run("resume")?;
    println!("ok: running");
    Ok(())
}

fn command_gdb(options: &OpenOcdOptions, port: u16) -> AppResult<()> {
    println!("GDB server: localhost:{port}");
    println!("arm-none-eabi-gdb -ex \"target extended-remote :{port}\" your.elf");
    let args = openocd_args(options, None, Some(port));
    let status = Command::new(&options.executable)
        .args(&args)
        .status()
        .map_err(|error| openocd_start_error(&options.executable, error))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(AppError::Exit(code)),
        None => Err(AppError::Runtime(
            "OpenOCD ended because of a signal".into(),
        )),
    }
}

fn command_raw(options: &OpenOcdOptions, command: &[String]) -> AppResult<()> {
    eprintln!("warning: raw bypasses all sam4e safety checks");
    let mut session = connect(options)?;
    let output = session.run(&command.join(" "))?;
    if !output.is_empty() {
        println!("{output}");
    }
    Ok(())
}

fn connect(options: &OpenOcdOptions) -> AppResult<Session> {
    let mut last = None;
    for attempt in 1..=3 {
        match Session::start(options).and_then(|mut session| {
            session.run_inner("halt")?;
            Ok(session)
        }) {
            Ok(session) => return Ok(session),
            Err(error) => {
                let retryable = error.retryable;
                last = Some(error.message);
                if !retryable || attempt == 3 {
                    break;
                }
                eprintln!(
                    "retrying: link attempt {attempt} failed; reconnecting ({}/3)",
                    attempt + 1
                );
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
    Err(AppError::Runtime(format!(
        "cannot reach the target: {}",
        last.unwrap_or_else(|| "unknown OpenOCD failure".into())
    )))
}

impl Session {
    fn start(options: &OpenOcdOptions) -> Result<Self, SessionError> {
        let port = free_port().map_err(|error| {
            SessionError::fatal(format!("could not reserve a local Tcl port: {error}"))
        })?;
        let args = openocd_args(options, Some(port), None);
        let mut child = Command::new(&options.executable)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                SessionError::fatal(format!(
                    "{}",
                    openocd_start_error(&options.executable, error)
                ))
            })?;
        let log = Arc::new(Mutex::new(VecDeque::with_capacity(64)));
        if let Some(stdout) = child.stdout.take() {
            drain_openocd(stdout, Arc::clone(&log), options.verbose);
        }
        if let Some(stderr) = child.stderr.take() {
            drain_openocd(stderr, Arc::clone(&log), options.verbose);
        }
        let mut session = Self {
            child,
            socket: None,
            log,
        };
        session.wait_for_tcl(port)?;
        Ok(session)
    }

    fn wait_for_tcl(&mut self, port: u16) -> Result<(), SessionError> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().map_err(|error| {
                SessionError::fatal(format!("could not read OpenOCD status: {error}"))
            })? {
                thread::sleep(Duration::from_millis(20));
                return Err(SessionError::fatal(format!(
                    "OpenOCD exited during startup ({status}){}",
                    self.log_tail(15)
                )));
            }
            match TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}")
                    .parse()
                    .expect("valid local address"),
                Duration::from_millis(250),
            ) {
                Ok(stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(300)))
                        .and_then(|_| stream.set_write_timeout(Some(Duration::from_millis(250))))
                        .map_err(|error| {
                            SessionError::fatal(format!("could not set Tcl timeout: {error}"))
                        })?;
                    self.socket = Some(stream);
                    return Ok(());
                }
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        }
        Err(SessionError::retryable(format!(
            "timed out waiting for OpenOCD{}",
            self.log_tail(15)
        )))
    }

    fn rpc(&mut self, script: &str) -> Result<String, SessionError> {
        let log = Arc::clone(&self.log);
        let socket = self
            .socket
            .as_mut()
            .ok_or_else(|| SessionError::retryable("OpenOCD Tcl socket is not connected"))?;
        socket
            .write_all(script.as_bytes())
            .and_then(|_| socket.write_all(&[RPC_EOM]))
            .map_err(|error| {
                SessionError::retryable(format!(
                    "lost the link to OpenOCD ({error}){}",
                    format_log_tail(&log, 8)
                ))
            })?;
        let mut output = Vec::new();
        loop {
            let mut chunk = [0_u8; 8192];
            let read = socket.read(&mut chunk).map_err(|error| {
                SessionError::retryable(format!(
                    "lost the link to OpenOCD ({error}){}",
                    format_log_tail(&log, 8)
                ))
            })?;
            if read == 0 {
                return Err(SessionError::retryable(format!(
                    "OpenOCD closed the connection{}",
                    format_log_tail(&log, 8)
                )));
            }
            output.extend_from_slice(&chunk[..read]);
            if output.last() == Some(&RPC_EOM) {
                output.pop();
                return Ok(String::from_utf8_lossy(&output).into_owned());
            }
        }
    }

    fn run_inner(&mut self, tcl: &str) -> Result<String, SessionError> {
        let raw = self.rpc(&rpc_wrapper(tcl))?;
        let (result, output) = raw.split_once(&format!("\n{MARK}\n")).ok_or_else(|| {
            SessionError::retryable(format!("invalid response from OpenOCD: {raw}"))
        })?;
        let output = output.trim().to_owned();
        if result.trim() != "0" {
            return Err(SessionError::retryable(if output.is_empty() {
                format!("OpenOCD command failed: {tcl}")
            } else {
                output
            }));
        }
        Ok(output)
    }

    fn run(&mut self, tcl: &str) -> AppResult<String> {
        self.run_inner(tcl)
            .map_err(|error| AppError::Runtime(error.message))
    }

    fn value(&mut self, expression: &str) -> AppResult<String> {
        self.rpc(expression)
            .map(|value| value.trim().to_owned())
            .map_err(|error| AppError::Runtime(error.message))
    }

    fn read32(&mut self, address: u32, count: usize) -> AppResult<Vec<u32>> {
        self.value(&format!("read_memory {address:#x} 32 {count}"))?
            .split_whitespace()
            .map(|word| {
                parse_number(word)
                    .and_then(|value| {
                        u32::try_from(value).map_err(|_| format!("word is too large: {word}"))
                    })
                    .map_err(AppError::Runtime)
            })
            .collect()
    }

    fn write32(&mut self, address: u32, words: &[u32]) -> AppResult<()> {
        let words = words
            .iter()
            .map(|word| format!("{word:#x}"))
            .collect::<Vec<_>>()
            .join(" ");
        self.run(&format!("write_memory {address:#x} 32 {{{words}}}"))?;
        Ok(())
    }

    fn log_tail(&self, count: usize) -> String {
        format_log_tail(&self.log, count)
    }
}

fn format_log_tail(log: &Arc<Mutex<VecDeque<String>>>, count: usize) -> String {
    let log = log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let start = log.len().saturating_sub(count);
    let lines = log.iter().skip(start).cloned().collect::<Vec<_>>();
    if lines.is_empty() {
        String::new()
    } else {
        format!("\n  {}", lines.join("\n  "))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut socket) = self.socket.take() {
            let _ = socket.write_all(b"shutdown\x1a");
            let _ = socket.shutdown(Shutdown::Both);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn drain_openocd<R: Read + Send + 'static>(
    reader: R,
    log: Arc<Mutex<VecDeque<String>>>,
    verbose: bool,
) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if verbose {
                eprintln!("openocd| {line}");
            }
            let mut log = log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if log.len() == 64 {
                log.pop_front();
            }
            log.push_back(line);
        }
    });
}

fn openocd_args(
    options: &OpenOcdOptions,
    tcl_port: Option<u16>,
    gdb_port: Option<u16>,
) -> Vec<OsString> {
    let mut commands = vec![
        "adapter driver cmsis-dap".to_owned(),
        "cmsis-dap vid_pid 0x03eb 0x2141".to_owned(),
    ];
    if let Some(serial) = &options.serial {
        commands.push(format!("adapter serial {}", tcl_literal(serial)));
    }
    commands.extend([
        format!("transport select {}", options.transport),
        "set CHIPNAME sam4e".to_owned(),
        "set WORKAREASIZE 0x10000".to_owned(),
        "source [find target/at91sam4XXX.cfg]".to_owned(),
        "set _FLASHNAME $_CHIPNAME.flash".to_owned(),
        "flash bank $_FLASHNAME at91sam4 0x00400000 0 1 1 $_TARGETNAME".to_owned(),
        "reset_config none".to_owned(),
        format!("adapter speed {}", options.speed),
        format!(
            "tcl_port {}",
            tcl_port.map_or_else(|| "disabled".to_owned(), |port| port.to_string())
        ),
        format!(
            "gdb_port {}",
            gdb_port.map_or_else(|| "disabled".to_owned(), |port| port.to_string())
        ),
        "telnet_port disabled".to_owned(),
    ]);
    commands
        .into_iter()
        .flat_map(|command| [OsString::from("-c"), OsString::from(command)])
        .collect()
}

fn openocd_start_error(executable: &Path, error: io::Error) -> AppError {
    AppError::Runtime(format!(
        "could not start OpenOCD '{}': {error}; install OpenOCD or use --openocd PATH",
        executable.display()
    ))
}

fn free_port() -> io::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

impl ChipInfo {
    fn read(session: &mut Session) -> AppResult<Self> {
        let ids = session.read32(CHIPID_CIDR, 2)?;
        if ids.len() != 2 {
            return Err(AppError::Runtime("CHIPID returned incomplete data".into()));
        }
        session.write32(EEFC_FCR, &[0x5a00_0000])?; // GETD: get flash descriptor.
        thread::sleep(Duration::from_millis(20));
        let mut descriptor = Vec::new();
        for _ in 0..4 {
            descriptor.push(read_frr(session)?);
        }
        let planes = usize::try_from(descriptor[3])
            .map_err(|_| AppError::Runtime("invalid EEFC plane count".into()))?;
        if planes == 0 || planes > 8 {
            return Err(AppError::Runtime(format!(
                "invalid EEFC plane count: {planes}"
            )));
        }
        for _ in 0..planes + 2 {
            descriptor.push(read_frr(session)?);
        }
        Ok(Self {
            cidr: ids[0],
            exid: ids[1],
            flash: parse_descriptor(&descriptor)?,
        })
    }

    fn name(&self) -> String {
        if self.exid == SAM4E8C_EXID {
            "SAM4E8C".into()
        } else {
            format!("unsupported (EXID {:#010x})", self.exid)
        }
    }

    fn require_sam4e8c(&self) -> AppResult<()> {
        if self.exid == SAM4E8C_EXID {
            Ok(())
        } else {
            Err(AppError::Runtime(format!(
                "target is not an ATSAM4E8C (EXID {:#010x})",
                self.exid
            )))
        }
    }

    fn cidr_flash_kb(&self) -> u32 {
        match (self.cidr >> 8) & 0xf {
            0 => 0,
            1 => 8,
            2 => 16,
            3 => 32,
            5 => 64,
            7 => 128,
            8 => 160,
            9 => 256,
            10 => 512,
            12 => 1024,
            14 => 2048,
            _ => 0,
        }
    }

    fn sram_kb(&self) -> u32 {
        const SRAM_KB: [u32; 16] = [
            48, 192, 384, 6, 24, 4, 80, 160, 8, 16, 32, 64, 128, 256, 96, 512,
        ];
        SRAM_KB[((self.cidr >> 16) & 0xf) as usize]
    }

    fn pages(&self) -> u32 {
        self.flash.size / self.flash.page_size
    }

    fn flash_end(&self) -> u32 {
        FLASH_BASE + self.flash.size
    }
}

fn require_sam4e8c(session: &mut Session) -> AppResult<()> {
    let exid = session
        .read32(CHIPID_CIDR, 2)?
        .get(1)
        .copied()
        .ok_or_else(|| AppError::Runtime("CHIPID returned incomplete data".into()))?;
    if exid == SAM4E8C_EXID {
        Ok(())
    } else {
        Err(AppError::Runtime(format!(
            "target is not an ATSAM4E8C (EXID {exid:#010x})"
        )))
    }
}

fn read_frr(session: &mut Session) -> AppResult<u32> {
    session
        .read32(EEFC_FRR, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Runtime("EEFC returned incomplete descriptor data".into()))
}

fn parse_descriptor(words: &[u32]) -> AppResult<FlashDescriptor> {
    if words.len() < 7 {
        return Err(AppError::Runtime(
            "EEFC flash descriptor is too short".into(),
        ));
    }
    let planes = words[3] as usize;
    let lock_index = 4_usize
        .checked_add(planes)
        .ok_or_else(|| AppError::Runtime("invalid EEFC flash descriptor".into()))?;
    if planes == 0 || lock_index + 1 >= words.len() {
        return Err(AppError::Runtime("invalid EEFC flash descriptor".into()));
    }
    if words[2] == 0 {
        return Err(AppError::Runtime("EEFC reports a zero page size".into()));
    }
    Ok(FlashDescriptor {
        id: words[0],
        size: words[1],
        page_size: words[2],
        planes: words[3],
        lock_regions: words[lock_index],
        lock_size: words[lock_index + 1],
    })
}

fn read_gpnvm(session: &mut Session) -> AppResult<[u8; 2]> {
    session.write32(EEFC_FCR, &[0x5a00_000d])?; // GGPB: get GPNVM bits.
    thread::sleep(Duration::from_millis(20));
    let word = read_frr(session)?;
    Ok([(word & 1) as u8, ((word >> 1) & 1) as u8])
}

fn print_gpnvm(bits: [u8; 2]) {
    println!("GPNVM");
    println!(
        "  0  security   {}  {}",
        bits[0],
        if bits[0] == 1 {
            "debug access disabled"
        } else {
            "unlocked"
        }
    );
    println!(
        "  1  boot_mode  {}  {}",
        bits[1],
        if bits[1] == 1 {
            "boot from Flash"
        } else {
            "boot from ROM (SAM-BA)"
        }
    );
}

fn load_config(explicit: Option<&Path>) -> AppResult<LoadedConfig> {
    let executable = env::current_exe()
        .map_err(|error| AppError::Usage(format!("cannot locate the sam4e executable: {error}")))?;
    let candidates = config_candidates(
        explicit,
        &executable,
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("HOME"),
        env::var_os("APPDATA"),
        cfg!(windows),
    );
    if explicit.is_some() && !candidates[0].is_file() {
        return Err(AppError::Usage(format!(
            "configuration file does not exist: {}",
            candidates[0].display()
        )));
    }
    let Some(path) = candidates.into_iter().find(|path| path.is_file()) else {
        return Ok(LoadedConfig {
            path: None,
            images: BTreeMap::new(),
        });
    };
    parse_config_file(&path)
}

fn config_candidates(
    explicit: Option<&Path>,
    executable: &Path,
    xdg: Option<OsString>,
    home: Option<OsString>,
    appdata: Option<OsString>,
    windows: bool,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path.to_owned()];
    }
    let mut paths = Vec::new();
    if let Some(parent) = executable.parent() {
        paths.push(parent.join("sam4e.toml"));
    }
    if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(xdg).join("sam4e").join("sam4e.toml"));
    }
    if windows {
        if let Some(appdata) = appdata.filter(|value| !value.is_empty()) {
            paths.push(PathBuf::from(appdata).join("sam4e").join("sam4e.toml"));
        }
    } else if let Some(home) = home.filter(|value| !value.is_empty()) {
        paths.push(
            PathBuf::from(home)
                .join(".config")
                .join("sam4e")
                .join("sam4e.toml"),
        );
    }
    paths.dedup();
    paths
}

fn parse_config_file(path: &Path) -> AppResult<LoadedConfig> {
    let text = fs::read_to_string(path)
        .map_err(|error| AppError::Usage(format!("cannot read {}: {error}", path.display())))?;
    let mut config: Config = toml::from_str(&text).map_err(|error| {
        let mut message = format!("cannot parse {}: {error}", path.display());
        if error.to_string().to_ascii_lowercase().contains("escape") {
            message.push_str("; use single quotes for Windows paths");
        }
        AppError::Usage(message)
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    for (name, image) in &mut config.images {
        if name.trim().is_empty() {
            return Err(AppError::Usage("image names must not be empty".into()));
        }
        if image.parts.is_empty() {
            return Err(AppError::Usage(format!("image '{name}' has no parts")));
        }
        for part in &mut image.parts {
            if part.file.as_os_str().is_empty() {
                return Err(AppError::Usage(format!(
                    "an image part in '{name}' has no file"
                )));
            }
            validate_image_address(&part.file, part.addr, &format!("image '{name}'"))?;
            if let Some(address) = part.addr {
                normalize_flash_addr(address)?;
            }
            if part.file.is_relative() {
                part.file = base.join(&part.file);
            }
        }
    }
    Ok(LoadedConfig {
        path: Some(path.to_owned()),
        images: config.images,
    })
}

fn prepare_flash_parts(
    target: &str,
    addr: Option<&str>,
    config: &LoadedConfig,
) -> AppResult<Vec<FlashPart>> {
    let mut parts = if let Some(image) = config.images.get(target) {
        if addr.is_some() {
            return Err(AppError::Usage(
                "--addr does not apply to a named image; edit sam4e.toml instead".into(),
            ));
        }
        image
            .parts
            .iter()
            .map(|part| {
                Ok(FlashPart {
                    file: part.file.clone(),
                    addr: part.addr.map(normalize_flash_addr).transpose()?,
                    format: image_format(&part.file)?,
                })
            })
            .collect::<AppResult<Vec<_>>>()?
    } else {
        let file = PathBuf::from(target);
        if !file.is_file() {
            let names = if config.images.is_empty() {
                "none defined".into()
            } else {
                config.images.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            return Err(AppError::Usage(format!(
                "'{target}' is not an existing file or named image ({names})"
            )));
        }
        let parsed_addr = addr.map(parse_flash_addr).transpose()?;
        validate_image_address(&file, parsed_addr.map(u64::from), "image file")?;
        vec![FlashPart {
            format: image_format(&file)?,
            file,
            addr: parsed_addr,
        }]
    };
    for part in &mut parts {
        if !part.file.is_file() {
            return Err(AppError::Usage(format!(
                "image file does not exist: {}",
                part.file.display()
            )));
        }
        part.file = fs::canonicalize(&part.file).map_err(|error| {
            AppError::Usage(format!("cannot resolve {}: {error}", part.file.display()))
        })?;
        path_string(&part.file)?;
    }
    Ok(parts)
}

fn image_format(path: &Path) -> AppResult<ImageFormat> {
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            AppError::Usage(format!(
                "image file has no supported extension: {}",
                path.display()
            ))
        })?;
    match extension.as_str() {
        "bin" => Ok(ImageFormat::Bin),
        "elf" | "axf" | "hex" | "ihex" | "s19" | "srec" => Ok(ImageFormat::SelfAddressed),
        _ => Err(AppError::Usage(format!(
            "unsupported image extension '.{extension}' for {}",
            path.display()
        ))),
    }
}

fn validate_image_address(path: &Path, addr: Option<u64>, context: &str) -> AppResult<()> {
    match (image_format(path)?, addr) {
        (ImageFormat::Bin, None) => Err(AppError::Usage(format!(
            "raw .bin in {context} requires an address"
        ))),
        (ImageFormat::SelfAddressed, Some(_)) => Err(AppError::Usage(format!(
            "an address is not allowed for self-addressed image {}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

fn plan_flash(parts: &[FlashPart], flash_end: u32) -> AppResult<FlashPlan> {
    if parts.is_empty() {
        return Err(AppError::Usage("image has no parts".into()));
    }
    let ranges = parts
        .iter()
        .map(|part| validate_part_range(part, flash_end))
        .collect::<AppResult<Vec<_>>>()?;
    if parts.len() == 1 {
        return Ok(FlashPlan::Single);
    }
    if parts.iter().any(|part| part.format != ImageFormat::Bin) {
        return Err(AppError::Usage(
            "a multi-part image must contain only raw .bin files with addresses".into(),
        ));
    }
    let ranges = ranges
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            AppError::Usage(
                "a multi-part image must contain only raw .bin files with addresses".into(),
            )
        })?;
    for (index, left) in ranges.iter().enumerate() {
        for right in &ranges[index + 1..] {
            if left.start < right.end && right.start < left.end {
                return Err(AppError::Usage(format!(
                    "multi-part image ranges overlap: {:#010x}-{:#010x} and {:#010x}-{:#010x}",
                    left.start,
                    left.end - 1,
                    right.start,
                    right.end - 1
                )));
            }
        }
    }
    Ok(FlashPlan::MultiBin(ranges))
}

fn validate_part_range(part: &FlashPart, flash_end: u32) -> AppResult<Option<FlashRange>> {
    let Some(address) = part.addr else {
        return Ok(None);
    };
    if !(FLASH_BASE..flash_end).contains(&address) {
        return Err(AppError::Usage(format!(
            "{address:#010x} is outside flash ({FLASH_BASE:#010x}-{flash_end:#010x})"
        )));
    }
    if part.format != ImageFormat::Bin {
        return Ok(None);
    }
    let size = fs::metadata(&part.file)
        .map_err(|error| AppError::Usage(format!("cannot read {}: {error}", part.file.display())))?
        .len();
    if size == 0 {
        return Err(AppError::Usage(format!(
            "raw image is empty: {}",
            part.file.display()
        )));
    }
    let end = u64::from(address) + size;
    if end > u64::from(flash_end) {
        return Err(AppError::Usage(format!(
            "{} ({size} B) at {address:#010x} runs past flash end {flash_end:#010x}",
            part.file.display()
        )));
    }
    Ok(Some(FlashRange {
        start: address,
        end: end as u32,
    }))
}

fn write_part(session: &mut Session, part: &FlashPart, erase: bool) -> AppResult<()> {
    let path = tcl_literal(&path_string(&part.file)?);
    let location = part
        .addr
        .map(|address| format!(" {address:#x} bin"))
        .unwrap_or_default();
    let label = part_label(part);
    println!("Programming {label}...");
    let erase = if erase { " erase" } else { "" };
    let output = session.run(&format!("flash write_image{erase} unlock {path}{location}"))?;
    println!("ok: wrote {label}");
    if !output.is_empty() {
        println!("  {}", output.replace('\n', "\n  "));
    }
    Ok(())
}

fn verify_part(session: &mut Session, part: &FlashPart) -> AppResult<()> {
    let path = tcl_literal(&path_string(&part.file)?);
    let address = part
        .addr
        .map(|address| format!(" {address:#x}"))
        .unwrap_or_default();
    println!("Verifying {}...", part.file.display());
    session.run(&format!("verify_image {path}{address}"))?;
    println!("ok: verified {}", part.file.display());
    Ok(())
}

fn part_label(part: &FlashPart) -> String {
    match part.addr {
        Some(address) => format!("{} @ {address:#010x}", part.file.display()),
        None => part.file.display().to_string(),
    }
}

fn parse_number(text: &str) -> Result<u64, String> {
    let mut value = text.trim().to_ascii_lowercase().replace('_', "");
    if value.is_empty() {
        return Err("number is empty".into());
    }
    let multiplier = match value.as_bytes().last().copied() {
        Some(b'k') => 1024,
        Some(b'm') => 1024_u64.pow(2),
        Some(b'g') => 1024_u64.pow(3),
        _ => 1,
    };
    if multiplier != 1 {
        value.pop();
    }
    let (radix, digits) = if let Some(hex) = value.strip_suffix('h') {
        (16, hex)
    } else if let Some(hex) = value.strip_prefix("0x") {
        (16, hex)
    } else if let Some(binary) = value.strip_prefix("0b") {
        (2, binary)
    } else if let Some(octal) = value.strip_prefix("0o") {
        (8, octal)
    } else {
        (10, value.as_str())
    };
    if digits.is_empty() {
        return Err(format!("invalid number: {text}"));
    }
    u64::from_str_radix(digits, radix)
        .map_err(|_| format!("invalid number: {text}"))?
        .checked_mul(multiplier)
        .ok_or_else(|| format!("number is too large: {text}"))
}

fn parse_absolute_addr(text: &str) -> AppResult<u32> {
    u32::try_from(parse_number(text).map_err(AppError::Usage)?)
        .map_err(|_| AppError::Usage("address is too large".into()))
}

fn parse_flash_addr(text: &str) -> AppResult<u32> {
    parse_number(text)
        .map_err(AppError::Usage)
        .and_then(normalize_flash_addr)
}

fn normalize_flash_addr(value: u64) -> AppResult<u32> {
    let value = if value < u64::from(FLASH_BASE) {
        value
            .checked_add(u64::from(FLASH_BASE))
            .ok_or_else(|| AppError::Usage("address is too large".into()))?
    } else {
        value
    };
    u32::try_from(value).map_err(|_| AppError::Usage("address is too large".into()))
}

fn parse_positive_usize(text: &str, name: &str) -> AppResult<usize> {
    let value = parse_number(text).map_err(AppError::Usage)?;
    if value == 0 {
        return Err(AppError::Usage(format!("{name} must be positive")));
    }
    usize::try_from(value).map_err(|_| AppError::Usage(format!("{name} is too large")))
}

fn parse_port(text: &str) -> Result<u16, String> {
    let port = text
        .parse::<u16>()
        .map_err(|_| "port must be between 1 and 65535".to_owned())?;
    if port == 0 {
        Err("port must be between 1 and 65535".into())
    } else {
        Ok(port)
    }
}

fn validate_tcl_value(value: Option<&str>, name: &str) -> AppResult<()> {
    if value.is_some_and(|value| value.contains('\0')) {
        Err(AppError::Usage(format!("{name} contains a null byte")))
    } else {
        Ok(())
    }
}

fn rpc_wrapper(tcl: &str) -> String {
    let command = tcl_literal(tcl);
    format!(
        "set __cmd {command}\nset __rc [catch [list capture $__cmd] __out]\nappend __rc \"\\n{MARK}\\n\" $__out"
    )
}

fn tcl_literal(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("\\$"),
            '[' => quoted.push_str("\\["),
            ']' => quoted.push_str("\\]"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{001a}' => quoted.push_str("\\x1a"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn absolute_path(path: &Path) -> AppResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|error| AppError::Usage(format!("cannot resolve {}: {error}", path.display())))
    }
}

fn path_string(path: &Path) -> AppResult<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| AppError::Usage(format!("path is not valid Unicode: {}", path.display())))
}

fn confirm(prompt: &str) -> AppResult<bool> {
    print!("{prompt} [y/N] ");
    io::stdout()
        .flush()
        .map_err(|error| AppError::Runtime(format!("cannot write prompt: {error}")))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| AppError::Runtime(format!("cannot read answer: {error}")))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("sam4e-test-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn ok<T>(result: AppResult<T>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn parses_supported_numbers() {
        assert_eq!(parse_number("0x400000").unwrap(), 0x400000);
        assert_eq!(parse_number("400000h").unwrap(), 0x400000);
        assert_eq!(parse_number("4M").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_number("1_024").unwrap(), 1024);
        assert!(parse_number("0x").is_err());
        assert!(parse_number("-1").is_err());
    }

    #[test]
    fn normalizes_flash_offsets() {
        assert_eq!(ok(parse_flash_addr("0x7a000")), 0x0047a000);
        assert_eq!(ok(parse_flash_addr("0x47a000")), 0x0047a000);
        assert!(parse_flash_addr("8G").is_err());
    }

    #[test]
    fn keeps_read_addresses_absolute() {
        assert_eq!(ok(parse_absolute_addr("0x00100000")), 0x00100000);
        assert!(parse_absolute_addr("8G").is_err());
    }

    #[test]
    fn enforces_image_address_rules() {
        assert!(validate_image_address(Path::new("x.bin"), None, "test").is_err());
        assert!(
            validate_image_address(Path::new("x.bin"), Some(FLASH_BASE.into()), "test").is_ok()
        );
        assert!(
            validate_image_address(Path::new("x.elf"), Some(FLASH_BASE.into()), "test").is_err()
        );
        assert!(validate_image_address(Path::new("x.elf"), None, "test").is_ok());
        assert!(image_format(Path::new("x.txt")).is_err());
    }

    #[test]
    fn discovers_sidecar_then_platform_config() {
        let paths = config_candidates(
            None,
            Path::new("/opt/sam4e/sam4e"),
            Some(OsString::from("/xdg")),
            Some(OsString::from("/home/user")),
            Some(OsString::from("C:\\AppData")),
            false,
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/opt/sam4e/sam4e.toml"),
                PathBuf::from("/xdg/sam4e/sam4e.toml"),
                PathBuf::from("/home/user/.config/sam4e/sam4e.toml")
            ]
        );
        let explicit = config_candidates(
            Some(Path::new("chosen.toml")),
            Path::new("/opt/sam4e/sam4e"),
            None,
            None,
            None,
            false,
        );
        assert_eq!(explicit, vec![PathBuf::from("chosen.toml")]);
    }

    #[test]
    fn uses_xdg_then_windows_appdata() {
        let paths = config_candidates(
            None,
            Path::new("/portable/sam4e.exe"),
            Some(OsString::from("/xdg")),
            Some(OsString::from("C:\\Users\\u")),
            Some(OsString::from("C:\\Users\\u\\AppData\\Roaming")),
            true,
        );
        assert_eq!(paths[0], PathBuf::from("/portable/sam4e.toml"));
        assert_eq!(paths[1], PathBuf::from("/xdg/sam4e/sam4e.toml"));
        assert!(paths[2].ends_with(Path::new("sam4e/sam4e.toml")));
    }

    #[test]
    fn parses_config_and_resolves_relative_files() {
        let dir = temp_dir();
        let path = dir.join("sam4e.toml");
        fs::write(
            &path,
            "[images.app]\nparts = [{ file = 'build/app.bin', addr = 0x400000 }]\n",
        )
        .unwrap();
        let config = ok(parse_config_file(&path));
        assert_eq!(
            config.images["app"].parts[0].file,
            dir.join("build/app.bin")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_invalid_config_shape() {
        let dir = temp_dir();
        let path = dir.join("sam4e.toml");
        fs::write(&path, "[images.app]\nparts = []\n").unwrap();
        assert!(parse_config_file(&path).is_err());
        fs::write(
            &path,
            "[images.app]\nparts = [{ file = 'app.elf', addr = 1 }]\n",
        )
        .unwrap();
        assert!(parse_config_file(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quotes_tcl_literals_without_rpc_end_bytes() {
        assert_eq!(
            tcl_literal("C:\\a $b [x] \"q\""),
            "\"C:\\\\a \\$b \\[x\\] \\\"q\\\"\""
        );
        let wrapper = rpc_wrapper("echo before\u{001a}after");
        assert!(!wrapper.as_bytes().contains(&RPC_EOM));
        assert!(wrapper.contains("\\x1a"));
    }

    #[test]
    fn plans_multi_part_flash_before_writes() {
        let dir = temp_dir();
        let first = dir.join("first.bin");
        let second = dir.join("second.bin");
        fs::write(&first, [1, 2, 3, 4]).unwrap();
        fs::write(&second, [5, 6, 7, 8]).unwrap();
        let parts = [
            FlashPart {
                file: first,
                addr: Some(FLASH_BASE),
                format: ImageFormat::Bin,
            },
            FlashPart {
                file: second,
                addr: Some(FLASH_BASE + 8),
                format: ImageFormat::Bin,
            },
        ];
        match ok(plan_flash(&parts, FLASH_BASE + 1024)) {
            FlashPlan::MultiBin(ranges) => assert_eq!(
                ranges,
                vec![
                    FlashRange {
                        start: FLASH_BASE,
                        end: FLASH_BASE + 4,
                    },
                    FlashRange {
                        start: FLASH_BASE + 8,
                        end: FLASH_BASE + 12,
                    }
                ]
            ),
            FlashPlan::Single => panic!("expected a multi-part plan"),
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_unsafe_multi_part_flash_plans() {
        let dir = temp_dir();
        let first = dir.join("first.bin");
        let second = dir.join("second.bin");
        let addressed = dir.join("third.elf");
        fs::write(&first, [0; 8]).unwrap();
        fs::write(&second, [0; 8]).unwrap();
        fs::write(&addressed, [0; 8]).unwrap();
        let overlap = [
            FlashPart {
                file: first,
                addr: Some(FLASH_BASE),
                format: ImageFormat::Bin,
            },
            FlashPart {
                file: second,
                addr: Some(FLASH_BASE + 4),
                format: ImageFormat::Bin,
            },
        ];
        assert!(plan_flash(&overlap, FLASH_BASE + 1024).is_err());
        let mixed = [
            FlashPart {
                file: addressed,
                addr: None,
                format: ImageFormat::SelfAddressed,
            },
            FlashPart {
                file: overlap[1].file.clone(),
                addr: Some(FLASH_BASE + 16),
                format: ImageFormat::Bin,
            },
        ];
        assert!(plan_flash(&mixed, FLASH_BASE + 1024).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn builds_embedded_openocd_config() {
        let options = OpenOcdOptions {
            executable: "openocd".into(),
            transport: Transport::Swd,
            speed: 400,
            serial: Some("A $B".into()),
            verbose: false,
        };
        let args = openocd_args(&options, Some(1234), None);
        let text = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("source [find target/at91sam4XXX.cfg]"));
        assert!(text.contains("adapter serial \"A \\$B\""));
        assert!(text.contains("tcl_port 1234"));
        assert!(text.contains("gdb_port disabled"));
        assert!(!text.contains(" -f "));
    }

    #[test]
    fn parses_descriptor_with_reported_plane_count() {
        let descriptor = ok(parse_descriptor(&[
            1,
            1024 * 1024,
            512,
            2,
            512 * 1024,
            512 * 1024,
            128,
            8192,
        ]));
        assert_eq!(
            descriptor,
            FlashDescriptor {
                id: 1,
                size: 1024 * 1024,
                page_size: 512,
                planes: 2,
                lock_regions: 128,
                lock_size: 8192,
            }
        );
        assert!(parse_descriptor(&[1, 2, 0, 1, 2, 3, 4]).is_err());
    }
}
