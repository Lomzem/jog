# sam4e

`sam4e` controls an ATSAM4E8C through an Atmel-ICE probe. It uses OpenOCD for
target access. The default link is SWD at 400 kHz.

## Requirements

Install OpenOCD. The tested version is 0.12. Make sure that `openocd` is on
`PATH` and that its script tree contains `target/at91sam4XXX.cfg`.

Use `sam4e.exe` on Windows x64. Install the Debian package on Ubuntu 24.04
amd64.

## Build release files

On a Linux host, start Docker and run:

```text
./build.sh
```

The script writes only these release files to `dist`:

```text
dist/sam4e-0.1.0-windows-x86_64.exe
dist/sam4e_0.1.0_amd64.deb
```

The version comes from `Cargo.toml`. Install the Debian package with:

```text
sudo apt install ./dist/sam4e_0.1.0_amd64.deb
```

The package installs `/usr/bin/sam4e` and requires OpenOCD.

## Start

Connect the Atmel-ICE and run:

```text
sam4e info
sam4e --help
```

Use `--openocd PATH` if OpenOCD is not on `PATH`. Use `--serial SERIAL` when
more than one Atmel-ICE is connected. Run `sam4e COMMAND --help` for command
options.

## Program flash

A self-addressed file contains its load address:

```text
sam4e flash build/app.elf
```

A raw BIN file does not contain an address. You must supply one:

```text
sam4e flash build/app.bin --addr 0x00400000
```

For flash and erase commands, an address below `0x00400000` is a flash offset.
Thus, `0x7a000` and `0x47a000` select the same flash address. The `read` command
always uses an absolute address.

By default, `flash` erases, writes, verifies, and resets. Do not use
`--no-verify` unless another check verifies the image. Use `--no-run` when the
target must remain halted.

A multi-part named image must contain only raw BIN files. Each part must have
an address, and the byte ranges must not overlap. `sam4e` erases all part ranges
before it writes the first part. It verifies all parts after all writes.

## Named images

The configuration file is optional. `sam4e` uses the first file in this list:

1. The path from `--config`.
2. `sam4e.toml` beside the executable.
3. `$XDG_CONFIG_HOME/sam4e/sam4e.toml` when `XDG_CONFIG_HOME` is set.
4. `$HOME/.config/sam4e/sam4e.toml` on Linux.
5. `%APPDATA%\sam4e\sam4e.toml` on Windows.

Relative image paths start from the directory that contains the configuration
file.

```toml
[images.application]
parts = [{ file = 'build/app.elf' }]

[images.loader]
parts = [
    { file = 'build/loader-1.bin', addr = 0x00400000 },
    { file = 'build/loader-2.bin', addr = 0x0047A000 },
]
```

Use single quotes for Windows paths. TOML interprets backslashes in double
quotes.

## Safety

Confirm the address and image before you program or erase flash.

`sam4e erase` asks for confirmation. Use `--yes` only in controlled automation.

Do not set GPNVM bit 0 unless you intend to disable JTAG and SWD. `sam4e`
requires `--force` before it sets this bit. The ERASE pin restores debug access
and erases all flash.

Do not use `raw` for normal work. It bypasses all address, image, and GPNVM
safety checks.

Use SWD unless the target requires JTAG. The tested Atmel-ICE firmware corrupted
large block transfers above approximately 500 kHz. Keep the 400 kHz default
until a test proves that another speed is reliable on the applicable probe.

## Temporary Python version

The Python version remains in `legacy/python` for hardware checks. Run
`legacy/python/sam4e` on Linux or `legacy\python\sam4e.cmd` on Windows. These
launchers use the root `sam4e.toml` file.

The Rust version intentionally rejects unsafe image combinations that the
Python version accepted. Complete the hardware checks before you remove the
Python version.
