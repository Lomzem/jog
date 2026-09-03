# sam4e

`sam4e` controls an ATSAM4E8C through an Atmel-ICE probe. It uses OpenOCD.
The default link is SWD at 400 kHz.

## Install

Install OpenOCD 0.12. Make sure that `openocd` is on `PATH`. Run:

```text
openocd --version
```

On Ubuntu 24.04 amd64, install the Debian package:

```text
sudo apt install ./dist/sam4e_0.1.0_amd64.deb
```

On Windows x64:

1. Install OpenOCD 0.12 and its Atmel-ICE USB driver.
2. Run `openocd --version`.
3. Rename `sam4e-0.1.0-windows-x86_64.exe` to `sam4e.exe`, or use the full file name.
4. Add its directory to `PATH` if you want to use `sam4e` from all directories.
5. Run `sam4e info`.

## First use

Connect the probe and target. Then run:

```text
sam4e info
sam4e --help
```

Use `--openocd PATH` if OpenOCD is not on `PATH`. Use `--serial SERIAL` if
more than one Atmel-ICE is connected. Run `sam4e COMMAND --help` for option
details.

## Common workflows

Program a raw BIN file:

```text
sam4e flash build/app.bin --addr 0x00400000
```

`sam4e` accepts raw BIN images because it can validate their complete range
before erase. It rejects ELF, HEX, and S-record images until it can validate
all load ranges.

By default, `flash` erases, writes, verifies, resets, and runs the target. Use
`--no-run` to keep the target halted. Use `--no-verify` only if a different
process verifies the image.

For `flash` and `erase`, an address below `0x00400000` is a flash offset. The
values `0x7a000` and `0x47a000` select the same address. `read` always uses an
absolute address.

Read-only commands restore a running target to its entry state. A target that
was halted stays halted.

## Named images

The configuration file is optional. `sam4e` uses the first file in this list:

1. The path from `--config`.
2. `sam4e.toml` in the current directory.
3. `sam4e.toml` beside the executable.
4. `$XDG_CONFIG_HOME/sam4e/sam4e.toml`.
5. `$HOME/.config/sam4e/sam4e.toml` on Linux.
6. `%APPDATA%\sam4e\sam4e.toml` on Windows.

Relative image paths start in the configuration file directory.

```toml
[images.application]
parts = [{ file = 'build/app.bin', addr = 0x00400000 }]

[images.loader]
parts = [
    { file = 'build/loader-1.bin', addr = 0x00400000 },
    { file = 'build/loader-2.bin', addr = 0x0047A000 },
]
```

List and program a named image:

```text
sam4e images
sam4e flash application
```

A multi-part image must contain addressed BIN files. The ranges must not
overlap. `sam4e` validates all parts, erases all ranges, writes all parts, and
then verifies all parts. Use single quotes for Windows paths in TOML.

## Safety

Check the image and address before you change flash.

`sam4e erase` needs terminal confirmation. Use `--yes` only in controlled
automation. The command fails before it starts OpenOCD if standard input is not
a terminal and `--yes` is absent.

GPNVM bit 0 disables JTAG and SWD. You must use `--force` to set it. The ERASE
pin restores debug access and erases all flash.

Do not use `raw` for normal work. It bypasses address, image, and GPNVM safety
checks.

Use SWD unless the target needs JTAG. The tested Atmel-ICE firmware corrupted
large transfers above approximately 500 kHz. Keep the 400 kHz default until a
test shows that a different speed is reliable with your probe.

## Build

Start Docker on a Linux host. Then run:

```text
./build.sh
```

The script runs the locked tests and creates:

```text
dist/sam4e-0.1.0-windows-x86_64.exe
dist/sam4e_0.1.0_amd64.deb
```

The version comes from `Cargo.toml`.

## Validation status

Automated tests and package checks do not access hardware. Validation with an
ATSAM4E8C, an Atmel-ICE, and OpenOCD 0.12 is pending. This includes entry-state
restore, ROM boot, flash failure recovery, and probe speed checks.
