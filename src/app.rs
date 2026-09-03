use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::env;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::flash::{
    FlashPlan, load_config, plan_flash, prepare_flash_parts, verify_part, write_part,
};
use crate::openocd::{
    OpenOcdOptions, connect, connect_read_only, openocd_args, openocd_start_error, path_string,
    tcl_literal,
};
use crate::target::{
    ChipInfo, FLASH_BASE, SRAM_BASE, normalize_flash_addr, read_gpnvm, require_sam4e8c,
};
use crate::{AppError, AppResult};

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

    #[arg(short = 's', long, global = true, default_value_t = 400, value_parser = clap::value_parser!(u32).range(1..), help = "Probe clock in kHz")]
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
    /// Program a named image or raw BIN file
    Flash {
        #[arg(help = "Image name or raw .bin file")]
        target: String,
        #[arg(short = 'a', long, help = "Flash address or offset for a raw .bin")]
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
        #[arg(value_enum)]
        mode: BootMode,
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
        #[arg(long, help = "Flash address or offset")]
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
        #[arg(help = "Number of bytes")]
        length: String,
        #[arg(short = 'o', long, help = "Write binary data to this file")]
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
        #[arg(short = 'p', long, default_value_t = 3333, value_parser = clap::value_parser!(u16).range(1..))]
        port: u16,
    },
    /// Advanced: run raw OpenOCD Tcl without sam4e safety checks
    Raw {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum BootMode {
    #[value(alias = "app")]
    Flash,
    #[value(alias = "samba", alias = "sam-ba", alias = "bootloader")]
    Rom,
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

pub(crate) fn run() -> AppResult<()> {
    execute(&Cli::parse())
}

fn execute(cli: &Cli) -> AppResult<()> {
    if cli
        .serial
        .as_ref()
        .is_some_and(|serial| serial.contains('\0'))
    {
        return Err(AppError::Usage("serial number contains a null byte".into()));
    }
    let options = OpenOcdOptions {
        executable: cli.openocd.clone(),
        transport: cli.transport.to_string(),
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
    let mut session = connect_read_only(options)?;
    let chip = ChipInfo::read(&mut session)?;
    let bits = read_gpnvm(&mut session)?;
    let state = session.entry_state();

    println!("SAM4E");
    println!("  Part          SAM4E8C");
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
    session.restore_entry_state()
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
            let location = format!("{:#010x}", normalize_flash_addr(part.addr)?);
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
    let parts = prepare_flash_parts(target, addr.map(parse_flash_addr).transpose()?, &config)?;
    let mut session = connect(options)?;
    let chip = ChipInfo::read(&mut session)?;
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
                session
                    .run(&erase_range_command(&range))
                    .map_err(AppError::flash_incomplete)?;
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

fn erase_range_command(range: &Range<u32>) -> String {
    format!(
        "flash erase_address pad unlock {:#x} {:#x}",
        range.start,
        range.end - range.start
    )
}

fn command_boot(options: &OpenOcdOptions, mode: &BootMode, reset_now: bool) -> AppResult<()> {
    let wanted = match mode {
        BootMode::Flash => 1,
        BootMode::Rom => 0,
    };
    let mut session = if reset_now {
        connect(options)?
    } else {
        connect_read_only(options)?
    };
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
        Ok(())
    } else {
        session.restore_entry_state()
    }
}

fn command_gpnvm(options: &OpenOcdOptions, command: &GpnvmCommand) -> AppResult<()> {
    match command {
        GpnvmCommand::Show => {
            let mut session = connect_read_only(options)?;
            require_sam4e8c(&mut session)?;
            print_gpnvm(read_gpnvm(&mut session)?);
            session.restore_entry_state()
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
    let whole = parsed_start.is_none() && parsed_end.is_none();
    let prompt = match (parsed_start, parsed_end) {
        (None, None) => "Erase all target flash?".to_owned(),
        (Some(a), None) => format!("Erase flash from {a:#010x} to the flash end?"),
        (None, Some(b)) => format!("Erase flash before {b:#010x}?"),
        (Some(a), Some(b)) if a < b => format!("Erase flash {a:#010x}-{:#010x}?", b - 1),
        (Some(a), Some(b)) => {
            return Err(AppError::Usage(format!(
                "erase start {a:#010x} must be below end {b:#010x}"
            )));
        }
    };
    if !yes && !confirm(&prompt)? {
        println!("Canceled. No flash was erased.");
        return Err(AppError::Exit(1));
    }

    let mut session = connect(options)?;
    let chip = ChipInfo::read(&mut session)?;
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
    println!("Erasing {what}...");
    if whole {
        session
            .run("flash erase_sector 0 0 last")
            .map_err(AppError::flash_incomplete)?;
    } else {
        session
            .run(&format!("flash erase_address {a:#x} {:#x}", b - a))
            .map_err(AppError::flash_incomplete)?;
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
    let mut session = connect_read_only(options)?;
    require_sam4e8c(&mut session)?;
    if let Some(path) = output {
        println!("Reading {length} bytes from {address:#010x}...");
        session.run(&format!(
            "dump_image {} {address:#x} {length:#x}",
            tcl_literal(&path_string(&path)?)
        ))?;
        println!("ok: wrote {length} bytes to {}", path.display());
        return session.restore_entry_state();
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
    session.restore_entry_state()
}

fn command_reset(options: &OpenOcdOptions, halt: bool) -> AppResult<()> {
    let mut session = connect(options)?;
    require_sam4e8c(&mut session)?;
    session.run(if halt { "reset halt" } else { "reset run" })?;
    println!("ok: reset ({})", if halt { "halted" } else { "running" });
    Ok(())
}

fn command_halt(options: &OpenOcdOptions) -> AppResult<()> {
    let mut session = connect(options)?;
    require_sam4e8c(&mut session)?;
    println!("ok: core {}", session.value("$_TARGETNAME curstate")?);
    Ok(())
}

fn command_resume(options: &OpenOcdOptions) -> AppResult<()> {
    let mut session = connect(options)?;
    require_sam4e8c(&mut session)?;
    session.run("resume")?;
    println!("ok: running");
    Ok(())
}

fn command_gdb(options: &OpenOcdOptions, port: u16) -> AppResult<()> {
    let mut session = connect_read_only(options)?;
    require_sam4e8c(&mut session)?;
    session.restore_entry_state()?;
    drop(session);

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

fn parse_positive_usize(text: &str, name: &str) -> AppResult<usize> {
    let value = parse_number(text).map_err(AppError::Usage)?;
    if value == 0 {
        return Err(AppError::Usage(format!("{name} must be positive")));
    }
    usize::try_from(value).map_err(|_| AppError::Usage(format!("{name} is too large")))
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

fn confirm(prompt: &str) -> AppResult<bool> {
    require_confirmation_terminals(io::stdin().is_terminal(), io::stderr().is_terminal())?;
    eprint!("{prompt} [y/N] ");
    io::stderr()
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

fn require_confirmation_terminals(input: bool, error_output: bool) -> AppResult<()> {
    if input && error_output {
        Ok(())
    } else {
        Err(AppError::Usage(
            "confirmation needs terminal input and error output. Use --yes for non-interactive use."
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_visible_interactive_confirmation() {
        assert!(require_confirmation_terminals(true, true).is_ok());
        assert!(require_confirmation_terminals(false, true).is_err());
        assert!(require_confirmation_terminals(true, false).is_err());
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
        assert_eq!(parse_flash_addr("0x7a000").unwrap(), 0x0047a000);
        assert_eq!(parse_flash_addr("0x47a000").unwrap(), 0x0047a000);
        assert!(parse_flash_addr("8G").is_err());
    }

    #[test]
    fn keeps_read_addresses_absolute() {
        assert_eq!(parse_absolute_addr("0x00100000").unwrap(), 0x00100000);
        assert!(parse_absolute_addr("8G").is_err());
    }

    #[test]
    fn pads_multi_part_flash_erase_ranges() {
        assert_eq!(
            erase_range_command(&(FLASH_BASE + 1..FLASH_BASE + 5)),
            "flash erase_address pad unlock 0x400001 0x4"
        );
    }

    #[test]
    fn boot_uses_canonical_values_and_compatible_aliases() {
        for mode in ["flash", "rom", "app", "samba"] {
            assert!(Cli::try_parse_from(["sam4e", "boot", mode]).is_ok());
        }
        assert!(Cli::try_parse_from(["sam4e", "boot", "invalid"]).is_err());
    }
}
