# jog

`jog` controls an ATSAM4E8C through an Atmel-ICE probe. It uses OpenOCD.
The default link is SWD at 400 kHz.

## Install

Install OpenOCD 0.12 or a current development build. Make sure that `openocd`
is on `PATH`. Run:

```text
openocd --version
```

Some development builds crash when `--serial` selects a CMSIS-DAP HID probe.
The installed build `0.12.0-01004-g9ea7f3d64-dirty` has this defect. Use a
build with the [upstream serial buffer fix](https://github.com/openocd-org/openocd/commit/e01e180f6248590348bad5c354c6b4e0cf1a956a).
The local hardware tests use this fixed build:

```text
target/release/jog --openocd backups/hardware-test/openocd-fixed/local/bin/openocd --transport jtag info
```

This local OpenOCD build is in the ignored test directory. It is not part of
the package. On another computer, use the path and probe serial for that computer.

On Ubuntu 24.04 amd64, install the Debian package:

```text
sudo apt install ./dist/jog_0.1.0_amd64.deb
```

On Windows x64:

1. Install OpenOCD 0.12 and its Atmel-ICE USB driver.
2. Run `openocd --version`.
3. Rename `jog-0.1.0-windows-x86_64.exe` to `jog.exe`, or use the full file name.
4. Add its directory to `PATH` if you want to use `jog` from all directories.
5. Run `jog info`.

## First use

Connect the probe and target. Then run:

```text
jog info
jog --help

# For a JTAG connection:
jog --transport jtag info
```

Use `--openocd PATH` if OpenOCD is not on `PATH`. Use `--serial SERIAL` if
more than one Atmel-ICE is connected. Run `jog COMMAND --help` for option
details.

## Common workflows

Program a raw BIN file:

```text
jog flash build/app.bin --addr 0x00400000
```

Program an ELF file through JTAG and keep the target halted:

```text
jog --transport jtag flash build/bootloader.elf --no-run
```

`jog` accepts raw BIN files and little-endian ARM ELF32 executable files
with `.elf` or `.axf` extensions. BIN files need `--addr`. ELF files use their
physical load addresses; do not give them `--addr`. The tool checks all load
ranges before erase. It writes the initial values for RAM to their flash load
addresses. It does not write ELF memory areas that have no file data.
HEX and S-record files are not supported.

By default, `flash` erases, writes, verifies, and resets the target to run.
It does not change the boot source. To select flash boot and reset, run:

```text
jog --transport jtag boot flash
```

Use `--no-run` with `flash` to keep the target halted. Use `--no-verify` only
if a different process verifies the image.

For `flash` and `erase`, an address below `0x00400000` is a flash offset. The
values `0x7a000` and `0x47a000` select the same address. `read` always uses an
absolute address.

Read-only commands restore a running target to its entry state. A target that
was halted stays halted.

After ROM boot, the tested device can stall debug access for about 18 seconds.
The tool allows a 20-second wait before its final connection attempt when
OpenOCD reports repeated debug-port stalls.

## Named images

The configuration file is optional. `jog` uses the first file in this list:

1. The path from `--config`.
2. `jog.toml` in the current directory.
3. `jog.toml` beside the executable.
4. `$XDG_CONFIG_HOME/jog/jog.toml`.
5. `$HOME/.config/jog/jog.toml` on Linux.
6. `%APPDATA%\jog\jog.toml` on Windows.

Relative image paths start in the configuration file directory. This example
uses generic file names. Set each path to the local image file.

```toml
[images.application]
parts = [{ file = 'build/app.bin', addr = 0x00400000 }]

[images.bootloader]
parts = [
    { file = 'build/bootloader-part1.bin', addr = 0x400000 },
    { file = 'build/bootloader-part2.bin', addr = 0x47a000 },
]

[images.bootloader_elf]
parts = [{ file = 'build/bootloader.elf' }]
```

List and program a named image:

```text
jog images
jog flash application
jog --transport jtag flash bootloader --no-run
```

A named image can contain BIN files with addresses and ELF files without
addresses. The load ranges must not overlap. `jog` checks all parts, erases
all ranges, writes all parts, and then verifies all parts. Use single quotes
for Windows paths in TOML.

The bootloader example writes pt1 at `0x400000` and pt2 at `0x47a000`.
It does not fill the gap between the parts. ELF load segments can also have
gaps. Whole flash sectors outside the load ranges keep their data. Erase
ranges extend to sector boundaries, so bytes outside a part in the same
boundary sector can be erased. Separate parts that share a sector are all
written after the erase operations.

## Safety

Check the image and address before you change flash.

`jog erase` needs terminal confirmation. Use `--yes` only in controlled
automation. The command fails before it starts OpenOCD if standard input is not
a terminal and `--yes` is absent.

GPNVM bit 0 disables JTAG and SWD. You must use `--force` to set it. The ERASE
pin restores debug access and erases all flash.

Do not use `raw` for normal work. It bypasses address, image, and GPNVM safety
checks.

Use the transport that matches the target wiring. JTAG and SWD passed the
hardware tests at 400 kHz. Higher speeds were not tested in this validation.

## Build

Start Docker on a Linux host. Then run:

```text
./build.sh
```

The script runs the locked tests and creates:

```text
dist/jog-0.1.0-windows-x86_64.exe
dist/jog_0.1.0_amd64.deb
```

The version comes from `Cargo.toml`.

## Validation status

Hardware tests used the connected Atmel-ICE and ATSAM4E8C.
JTAG and SWD passed at 400 kHz. Tests covered all three supplied ELF files,
both bootloader BIN parts, readback, the gap, control commands, ROM boot,
GPNVM, range erase, full erase, and a GDB protocol connection.

See [the hardware test record](docs/hardware-validation.md) for the results
and limits. The Linux release build, unit tests, command tests, Clippy, and
Windows compile checks passed. Docker was not running, so package builds
were not run in this session.
