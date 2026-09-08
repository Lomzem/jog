# Hardware test record

The tests used the laptop USB connection, Atmel-ICE, and connected ATSAM4E8C.
The chip reported 1 MiB of flash, 512-byte pages, 128 lock regions of 8 KiB,
and 128 KiB of SRAM.

## Software

- `jog`: local source and Linux release build.
- Installed OpenOCD: `0.12.0-01004-g9ea7f3d64-dirty`.
- Fixed OpenOCD: `0.12.0+dev-gbedefa2`, built in
  `backups/hardware-test/openocd-fixed/local/`.
- Probe clock: 400 kHz, with JTAG and SWD.

The installed OpenOCD rejected `cmsis-dap vid_pid`. The CLI now supports
the USB selection command names in old and new OpenOCD builds.

The installed OpenOCD also crashed with `--serial`. Its HID code passed a
byte count to `mbstowcs`, which needs a wide-character count. The fixed build
contains the [upstream correction](https://github.com/openocd-org/openocd/commit/e01e180f6248590348bad5c354c6b4e0cf1a956a).
Serial selection passed with that build. No system OpenOCD files were changed.

## Flash tests

The files came from the ignored test directory. The table uses generic image
labels; it does not contain the original file names.

| Image | Transport | Result |
| --- | --- | --- |
| Bootloader part 1 at `0x400000` and bootloader part 2 at `0x47a000` | JTAG | Programming, verification, and exact byte readback passed |
| Bootloader ELF | JTAG | All six load ranges programmed and verified |
| Application ELF | SWD | Both flash load ranges programmed and verified |
| Application ELF variant | JTAG | Both flash load ranges programmed and verified |

After the split BIN write, the gap between the parts matched the original
backup. The test included programmed data in sectors outside both image
ranges. Their data stayed unchanged.

The partial boundary sector after part 1 was already erased. This test does
not establish preservation of programmed bytes in a partial boundary sector.
Erase operations can change those bytes, as described in the README.

## Command tests

| Command or behavior | Result |
| --- | --- |
| `info`, JTAG and SWD | Chip, geometry, and target state read correctly |
| `images` with two addressed BIN parts | Paths and addresses correct |
| `read` to a file | Full flash backup and split-image readback passed |
| Unaligned `read 0x400001 7` | Exact seven bytes matched the backup |
| `halt`, `resume`, `reset --halt`, `reset` | Target state checks passed |
| `info` with a running or halted target | Entry state restored |
| `boot rom` and reconnect | Passed with the longer retry interval |
| `boot rom --no-reset` | Passed |
| `boot flash` | Flash boot selected and target reset successfully |
| `gpnvm show`, `set 1`, `clear 1`, `clear 0` | Readback checks passed |
| `erase --start 0 --end 0x8000 --yes` | All 32 KiB read back as `0xff` |
| `erase --yes` | All 1 MiB verified as `0xff` |
| `raw` | Tcl commands and image verification passed |
| `gdb --port PORT` | Remote protocol `qSupported` returned capabilities |
| `--serial` with fixed OpenOCD | Passed |

The ROM stalled debug access for about 18 seconds after reset. Lower clock
speed and a delayed disconnect did not prevent the stall. The tool now waits
20 seconds before the final attempt after repeated AP/WAIT errors. Immediate
reconnect tests then passed without a cable or power change.

## Backup and limits

The original 1 MiB backup is `backups/hardware-test/original-flash.bin`.
After testing, the full backup was restored and verified. A separate 1 MiB
readback matched the backup byte for byte. Flash boot was restored and the
target was left running. Two successive `info` calls confirmed that state.

Detailed logs and readback files are in `backups/hardware-test/`. This
directory and `test-elf-do-not-commit/` are ignored by Git and Docker.

Security bit 0 was not set. That operation disables debug access. GDB tests
checked the protocol connection, not a full source debug session. Firmware
application behavior and speeds above 400 kHz were not tested. Flash failure
recovery after cable removal was not tested.

Automated checks passed: 33 unit tests, six command integration tests,
formatting, Clippy with warnings denied, Linux release build, and Windows
compile checks. Docker package builds and Windows hardware tests were not run.
