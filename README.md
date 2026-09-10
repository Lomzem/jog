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

For an ELF or Intel HEX file, the file supplies the addresses:

```text
jog flash build/app.elf
jog flash build/app.hex
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

Supported formats are raw BIN, little-endian ARM ELF32 with `.elf` or `.axf`
extensions, and Intel HEX with `.hex`, `.ihex`, or `.mcs` extensions.
Do not use `--addr` with ELF or Intel HEX files.
S-record is not supported.

Intel HEX files can contain several data ranges with absolute addresses.
`jog` checks the complete image before erasing. It rejects malformed records,
invalid checksums, unsupported record types, data outside the target's flash,
and overlapping data, even when the overlapping bytes match.
Intel HEX start-address records do not change how `jog` resets and runs the target.

**Data loss:** A flash erase operates on whole sectors.
It can also erase data outside your image in the same sector.
Gaps between Intel HEX data ranges follow the same erase behavior.
Do not rely on data in those gaps surviving a flash operation.

## Save image names in jog.toml

The optional `jog.toml` file stores image names, paths, and addresses.
Set connection options, such as JTAG, on the command line.

To create an empty configuration file with commented examples, run:

```text
jog --config-init
```

This creates `jog.toml` in the directory shown by `jog --config-dir`.
It creates missing directories and reports an error if the file already exists.
To create the file in the current directory instead, run:

```text
jog --config-init --config jog.toml
```

Edit the file to add image names. For example:

```toml
[images.application]
description = 'Main application'
parts = [{ file = 'build/app.elf' }]

[images.application_bin]
parts = [{ file = 'build/app.bin', addr = 0x00400000 }]

[images.application_hex]
parts = [{ file = 'build/app.hex' }]
```

- Image names such as `application` are names that you choose.
- `description` is optional text shown by `jog images`.
- `parts` lists the files to write for that name.
- `file` is the path to a firmware file.
- `addr` is required for BIN files. Omit it for ELF and Intel HEX files.

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
You can mix BIN, ELF, and Intel HEX parts. Each ELF or Intel HEX part supplies
its own addresses. `jog` rejects overlaps within a file or between parts
before erasing.
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

To print the user configuration directory and exit, run:

```text
jog --config-dir
```

This command prints a path even if the directory does not exist.


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

`read` requires an absolute address. For BIN addresses and `erase` ranges,
you can also use offsets: `0` means `0x00400000`.
Addresses embedded in ELF and Intel HEX files are always absolute.

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

The script runs Linux tests and compiles Windows tests. It replaces `dist/`
with the Linux executable, Ubuntu package, and Windows executable.
File names use the version in `Cargo.toml`.

### Automatic builds

GitHub Actions runs the same build on each push and pull request.
It also runs the Windows executable with `--version` and `--help` on Windows.
You can also start the **Build** workflow manually from the **Actions** tab.
Open a successful run and download the files from **Artifacts**:

- `jog-linux-x86_64`: Linux executable, built on Ubuntu 24.04.
- `jog-windows-x86_64`: Windows executable.
- `jog-ubuntu-24.04-amd64`: Ubuntu 24.04 package.

Extract the downloaded archive. On Linux, run
`chmod +x jog-*-linux-x86_64` to give the executable permission to run.
OpenOCD must be installed separately.

### Publish a release

Install `just` and the GitHub CLI, then authenticate with `gh auth login`.
Run these commands in Bash on Linux, macOS, or Windows with Git Bash.
Your `origin` remote must point to the GitHub repository you want to release.

Update the version in `Cargo.toml` and `Cargo.lock`, then commit your changes.
Tag the current commit and push the tag:

```text
just tag v0.1.0
```

The tag must match the Cargo version, and the working tree must be clean.
Pushing the tag starts the Build workflow. Find its run ID in the Actions run
URL or with `gh run list --workflow build.yml --branch v0.1.0`.
Wait for the entire run to succeed, including the Windows check, then publish:

```text
just release v0.1.0 RUN_ID
```

Replace `RUN_ID` with the numeric run ID. The command checks that the successful
Build run used the tagged commit, downloads all three artifacts to a temporary
directory, and publishes their files with generated release notes.
You can also use a successful push or manual Build run of the same commit.
Pull request runs are rejected.

The release command requires the tag to exist on `origin` and refuses to replace
an existing release. If a tag push fails, rerun `just tag` from the same commit.
For a failed build, fix the problem before choosing a new version and tag.
If artifact download fails, retry the release command once the artifacts are available.
If an upload fails after GitHub creates a draft release, inspect that draft in
GitHub Releases. Delete the incomplete draft while keeping its tag, then retry.
See the [GitHub CLI release documentation](https://cli.github.com/manual/gh_release_create)
for release creation options.
