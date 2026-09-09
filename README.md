# jog

`jog` is a command-line tool for the ATSAM4E8C microcontroller.
It uses an Atmel-ICE probe and OpenOCD to connect to your board.
**Target** means the microcontroller on your board.

With `jog`, you can:

- Show target information and check the connection.
- Write firmware to flash memory and verify the data.
- Save names for firmware files in a TOML configuration file.
- Read memory, erase flash, and reset or stop the target.
- Select flash or ROM boot, or start a GDB server for debugging.

## Install

You need an Atmel-ICE probe, a target board, and OpenOCD 0.12 or later.
On Windows, also install the Atmel-ICE USB driver for OpenOCD.
Make sure that this command works in your terminal:

```text
openocd --version
```

**Ubuntu 24.04, amd64:** Install the package from the `dist` directory:

```text
sudo apt install ./dist/jog_0.1.0_amd64.deb
```

**Windows, x64:** Rename `jog-0.1.0-windows-x86_64.exe` to `jog.exe`.
Add its directory to `PATH` to use `jog` from any directory.

To create these files from source, see [Build from source](#build-from-source).

## First use

1. Connect the Atmel-ICE to your computer and target board.
2. Supply power to the target board.
3. Check the connection:

   ```text
   jog info
   ```

The default connection is SWD at 400 kHz. If your board uses JTAG, add
`--transport jtag` to your commands:

```text
jog --transport jtag info
```

## Write firmware

An **image** is firmware data. Replace the example paths with your own paths.

For an ELF file, the file supplies the addresses:

```text
jog flash build/app.elf
```

For a BIN file, you must supply its flash address:

```text
jog flash build/app.bin --addr 0x00400000
```

Check the address required by your firmware.
`0x00400000` is the start of flash on this target.

By default, `flash` erases the required flash areas, writes the image,
verifies the data, and resets the target to run.
To keep the target stopped after the write, add `--no-run`:

```text
jog flash build/app.elf --no-run
```

`flash` does not change the boot source. To select flash boot and reset, run:

```text
jog boot flash
```

Supported formats: raw BIN and little-endian ARM ELF32 (`.elf` or `.axf`).
Do not use `--addr` with ELF or AXF. HEX and S-record are not supported.

**Data loss:** A flash erase operates on whole sectors.
It can also erase data outside your image in the same sector.

## Save image names in jog.toml

The optional `jog.toml` file stores image names, paths, and addresses.
Set connection options, such as JTAG, on the command line.

Create or edit `jog.toml` in the directory where you run `jog`:

```toml
[images.application]
description = 'Main application'
parts = [{ file = 'build/app.elf' }]

[images.application_bin]
parts = [{ file = 'build/app.bin', addr = 0x00400000 }]
```

- `application` and `application_bin` are names that you choose.
- `description` is optional text shown by `jog images`.
- `parts` lists the files to write for that name.
- `file` is the path to a firmware file.
- `addr` is required for BIN files. Omit it for ELF and AXF files.

Relative file paths start from the directory that contains `jog.toml`.
Use single quotes around paths, especially Windows paths.

List the names, then write one image:

```text
jog images
jog flash application
```

For image names, set addresses in TOML. Do not add `--addr`.

### Write several files with one name

Add each file to the same `parts` list:

```toml
[images.combined]
parts = [
    { file = 'build/part1.bin', addr = 0x00400000 },
    { file = 'build/part2.bin', addr = 0x00420000 },
]
```

Check the addresses. The file data must not overlap.
Run `jog flash combined` to write and verify all parts.

### Use a different configuration file

Select a file with `--config`:

```text
jog --config config/images.toml flash application
```

This file must exist. Without `--config`, `jog` uses the first file found
in this order:

1. `jog.toml` in the current directory.
2. `jog.toml` beside the `jog` executable.
3. `$XDG_CONFIG_HOME/jog/jog.toml`, if that variable is set.
4. `$HOME/.config/jog/jog.toml` on Linux, or `%APPDATA%\jog\jog.toml` on Windows.


## Other commands

| Task | Command |
| --- | --- |
| Show target information | `jog info` |
| Read 256 bytes from flash | `jog read 0x00400000 256` |
| Save those bytes to a file | `jog read 0x00400000 256 --out readback.bin` |
| Reset and run the target | `jog reset` |
| Reset and keep the target stopped | `jog reset --halt` |
| Stop the target | `jog halt` |
| Continue from the stopped state | `jog resume` |
| Select ROM (SAM-BA) boot and reset | `jog boot rom` |
| Start a GDB server on port 3333 | `jog gdb` |
| Erase all flash | `jog erase` |

`read` requires an absolute address. For `flash` and `erase`,
you can also use offsets: `0` means `0x00400000`.

`jog erase` asks for confirmation. For a range, use `--start` and `--end`.
The end address is not included.

For advanced commands, see `jog gpnvm --help` and `jog raw --help`.
GPNVM bit 0 disables debug access. `raw` skips safety checks.

## Help and connection options

```text
jog --help
jog flash --help
```

| Need | Option example |
| --- | --- |
| Use JTAG | `jog --transport jtag info` |
| Select one of several probes | `jog --serial SERIAL info` |
| Set the OpenOCD path | `jog --openocd /path/to/openocd info` |
| Show the OpenOCD log | `jog --verbose info` |

If OpenOCD crashes with `--serial`, see the
[known issue and correction](docs/hardware-validation.md#software).
A connection after ROM boot can take about 20 seconds.

## Build from source

On a Linux computer with Docker installed and running, run:

```text
./build.sh
```

The script runs tests and replaces `dist/` with the Ubuntu package and
Windows executable. File names use the version in `Cargo.toml`.
