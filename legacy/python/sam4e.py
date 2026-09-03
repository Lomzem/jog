#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["typer>=0.12", "rich>=13.7"]
# ///
"""
sam4e -- a friendly CLI for an ATSAM4E over an Atmel-ICE.

OpenOCD drives the Atmel-ICE as a CMSIS-DAP probe. One OpenOCD process is
started per invocation and driven over its TCL RPC port, so command results
come back as values instead of being scraped out of a log.
"""

from __future__ import annotations

import atexit
import contextlib
import functools
import sys
import re
import shutil
import socket
import subprocess
import threading
import time
import tomllib
from pathlib import Path
from typing import Optional

import typer
from rich.console import Console
from rich.panel import Panel
from rich.table import Table

HERE = Path(__file__).resolve().parent
CFG = HERE / "sam4e.cfg"
IMAGES_TOML = HERE.parent.parent / "sam4e.toml"

# Windows consoles often default to cp1252, which cannot encode the box-drawing
# and spinner glyphs rich emits.
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, OSError):
        pass

console = Console()
err = Console(stderr=True)


def status(message: str):
    """A spinner on a real terminal; nothing at all when output is redirected."""
    if not console.is_terminal:
        console.print(f"[dim]{message}[/dim]")
        return contextlib.nullcontext()
    return console.status(message, spinner="line")   # ASCII: safe on any codepage

FLASH_BASE = 0x00400000
SRAM_BASE = 0x20000000
CHIPID_CIDR = 0x400E0740
EEFC_BASE = 0x400E0A00
EEFC_FCR = EEFC_BASE + 0x04
EEFC_FRR = EEFC_BASE + 0x0C

GPNVM_BITS = {
    0: ("security", "Security bit -- locks out JTAG/SWD until a full ERASE"),
    1: ("boot_mode", "BOOT_MODE: 1 = boot from Flash, 0 = boot from ROM (SAM-BA)"),
}

NVPSIZ = {0: 0, 1: 8, 2: 16, 3: 32, 5: 64, 7: 128, 8: 160,
          9: 256, 10: 512, 12: 1024, 14: 2048}
SRAMSIZ = {0: 48, 1: 192, 2: 384, 3: 6, 4: 24, 5: 4, 6: 80, 7: 160,
           8: 8, 9: 16, 10: 32, 11: 64, 12: 128, 13: 256, 14: 96, 15: 512}
EXID_NAMES = {
    0x00120200: "SAM4E16E", 0x00120201: "SAM4E16C",
    0x00120208: "SAM4E8E", 0x00120209: "SAM4E8C",
}

ELF_LIKE = (".elf", ".axf", ".hex", ".ihex", ".s19", ".srec")
RPC_EOM = b"\x1a"
MARK = "---SAM4E-OUT---"


class OpenOCDError(RuntimeError):
    pass


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def parse_int(text: str | int) -> int:
    """Accept 0x400000, 400000h, 4M, 512k, or plain decimal."""
    if isinstance(text, int):
        return text
    t = text.strip().lower().replace("_", "")
    mult = 1
    if t and t[-1] in "kmg":
        mult = {"k": 1024, "m": 1024 ** 2, "g": 1024 ** 3}[t[-1]]
        t = t[:-1]
    if t.endswith("h"):
        return int(t[:-1], 16) * mult
    if t.startswith(("0x", "0b", "0o")):
        return int(t, 0) * mult
    return int(t, 10) * mult


def as_flash_addr(value: str | int) -> int:
    """Treat a bare offset below the flash base as flash-relative."""
    a = parse_int(value)
    return a + FLASH_BASE if a < FLASH_BASE else a


def _image_kind(path: Path) -> str:
    suffix = path.suffix.lower()
    if suffix == ".bin":
        return "bin"
    if suffix in ELF_LIKE:
        return "self-addressed"
    raise ValueError(f"unsupported image extension '{suffix or '<none>'}' for {path}")


def _validate_image_part(path: Path, addr: Optional[int]) -> str:
    kind = _image_kind(path)
    if kind == "bin" and addr is None:
        raise ValueError(f"raw .bin requires an address: {path}")
    if kind == "self-addressed" and addr is not None:
        raise ValueError(f"an address is not allowed for self-addressed image: {path}")
    return kind


def _self_check() -> None:
    assert _validate_image_part(Path("app.bin"), FLASH_BASE) == "bin"
    assert _validate_image_part(Path("app.elf"), None) == "self-addressed"
    for path, addr in ((Path("app.bin"), None), (Path("app.elf"), FLASH_BASE),
                       (Path("app.txt"), None)):
        try:
            _validate_image_part(path, addr)
        except ValueError:
            pass
        else:
            raise AssertionError(f"accepted unsafe image part: {path}")


class Session:
    """A running OpenOCD process, driven over its TCL RPC socket."""

    def __init__(self, transport: str = "swd", speed: int = 400,
                 serial: Optional[str] = None, verbose: bool = False):
        self.verbose = verbose
        self.log: list[str] = []
        self.proc: Optional[subprocess.Popen] = None
        self.sock: Optional[socket.socket] = None
        self.tcl_port = _free_port()

        openocd = shutil.which("openocd")
        if not openocd:
            raise OpenOCDError("openocd was not found on PATH.")
        if not CFG.exists():
            raise OpenOCDError(f"missing OpenOCD config: {CFG}")

        cmd = [openocd,
               "-c", f"set TRANSPORT {transport}",
               "-c", f"set SPEED {speed}"]
        if serial:
            cmd += ["-c", f"set ADAPTER_SERIAL {serial}"]
        cmd += ["-c", f"tcl_port {self.tcl_port}",
                "-c", f"gdb_port {_free_port()}",
                "-c", "telnet_port disabled",
                "-f", str(CFG)]

        self.proc = subprocess.Popen(
            cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, encoding="utf-8", errors="replace", bufsize=1,
            creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0),
        )
        threading.Thread(target=self._drain, daemon=True).start()
        atexit.register(self.close)
        self._connect()

    def _drain(self) -> None:
        assert self.proc and self.proc.stdout
        for line in self.proc.stdout:
            self.log.append(line.rstrip())
            if self.verbose:
                err.print(f"[dim]openocd|[/dim] {line.rstrip()}")

    def _connect(self, timeout: float = 20.0) -> None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc and self.proc.poll() is not None:
                raise OpenOCDError("OpenOCD exited during startup:\n  "
                                   + "\n  ".join(self.log[-15:]))
            try:
                s = socket.create_connection(("127.0.0.1", self.tcl_port), 1.0)
                s.settimeout(300.0)
                self.sock = s
                return
            except OSError:
                time.sleep(0.15)
        raise OpenOCDError("timed out waiting for OpenOCD:\n  "
                           + "\n  ".join(self.log[-15:]))

    def _rpc(self, script: str) -> str:
        assert self.sock
        try:
            self.sock.sendall(script.encode() + RPC_EOM)
            buf = bytearray()
            while not buf.endswith(RPC_EOM):
                chunk = self.sock.recv(8192)
                if not chunk:
                    raise OpenOCDError("OpenOCD closed the connection.")
                buf += chunk
        except OSError as e:
            raise OpenOCDError(f"lost the link to OpenOCD ({e}).\n  "
                               + "\n  ".join(self.log[-8:])) from e
        return buf[:-1].decode("utf-8", "replace")

    def run(self, tcl: str, check: bool = True) -> str:
        """Run an OpenOCD command; return its console output."""
        wrapper = (f'set __rc [catch {{capture {{{tcl}}}}} __out]\n'
                   f'append __rc "\\n{MARK}\\n" $__out')
        raw = self._rpc(wrapper)
        rc, _, out = raw.partition(f"\n{MARK}\n")
        out = out.strip()
        if check and rc.strip() != "0":
            raise OpenOCDError(out or f"command failed: {tcl}")
        return out

    def value(self, tcl_expr: str) -> str:
        """Evaluate a TCL expression and return its value."""
        return self._rpc(tcl_expr).strip()

    def read32(self, addr: int, count: int = 1) -> list[int]:
        return [int(w, 0) for w in
                self.value(f"read_memory {addr:#x} 32 {count}").split()]

    def write32(self, addr: int, words: list[int]) -> None:
        self.run(f"write_memory {addr:#x} 32 {{{' '.join(f'{w:#x}' for w in words)}}}")

    def halt(self) -> None:
        self.run("halt")

    def close(self) -> None:
        if self.sock:
            try:
                self._rpc("shutdown")
            except Exception:
                pass
            try:
                self.sock.close()
            except Exception:
                pass
            self.sock = None
        if self.proc and self.proc.poll() is None:
            try:
                self.proc.terminate()
                self.proc.wait(timeout=5)
            except Exception:
                self.proc.kill()
        self.proc = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


class ChipInfo:
    """Identity and flash geometry, read straight from CHIPID and the EEFC."""

    def __init__(self, s: Session):
        s.halt()
        self.cidr, self.exid = s.read32(CHIPID_CIDR, 2)
        s.write32(EEFC_FCR, [0x5A000000])           # GETD -- get flash descriptor
        time.sleep(0.02)
        d = [s.read32(EEFC_FRR)[0] for _ in range(8)]
        self.fl_id, self.fl_size, self.page_size, self.nb_plane = d[0:4]
        self.nb_lock, self.lock_size = d[5], d[6]

    @property
    def name(self) -> str:
        return EXID_NAMES.get(self.exid, f"unknown (EXID {self.exid:#010x})")

    @property
    def cidr_flash_kb(self) -> int:
        return NVPSIZ.get((self.cidr >> 8) & 0xF, 0)

    @property
    def sram_kb(self) -> int:
        return SRAMSIZ.get((self.cidr >> 16) & 0xF, 0)

    @property
    def pages(self) -> int:
        return self.fl_size // self.page_size if self.page_size else 0

    @property
    def flash_end(self) -> int:
        return FLASH_BASE + self.fl_size


def read_gpnvm(s: Session) -> dict[int, int]:
    """Read GPNVM bits via the EEFC, independent of OpenOCD's chip table."""
    s.halt()
    s.write32(EEFC_FCR, [0x5A00000D])               # GGPB -- get GPNVM bits
    time.sleep(0.02)
    word = s.read32(EEFC_FRR)[0]
    return {n: (word >> n) & 1 for n in GPNVM_BITS}


def load_images() -> dict:
    if not IMAGES_TOML.exists():
        return {}
    try:
        with IMAGES_TOML.open("rb") as fh:
            images = tomllib.load(fh).get("images", {})
    except tomllib.TOMLDecodeError as e:
        hint = ""
        if "hex value" in str(e) or "escape" in str(e).lower():
            hint = ("\n\nA Windows path in double quotes is the usual cause: TOML reads "
                    "backslash\nescapes inside \"...\", so C:\\Users trips on \\U. Use single "
                    "quotes instead --\n'C:\\Users\\...' is a literal string and needs no escaping.")
        err.print(Panel(f"{IMAGES_TOML}\n\n{e}{hint}",
                        title="[red]cannot parse sam4e.toml", border_style="red"))
        raise typer.Exit(2)

    for name, spec in images.items():
        if not isinstance(spec, dict) or not spec.get("parts"):
            err.print(f"[red]image '{name}' in sam4e.toml has no 'parts'.")
            raise typer.Exit(2)
        for part in spec["parts"]:
            if "file" not in part:
                err.print(f"[red]a part of image '{name}' is missing 'file'.")
                raise typer.Exit(2)
    return images


app = typer.Typer(add_completion=False, no_args_is_help=True,
                  rich_markup_mode="rich",
                  help="Friendly CLI for an ATSAM4E over an Atmel-ICE (CMSIS-DAP).")
gpnvm_app = typer.Typer(no_args_is_help=True, help="Read and change GPNVM bits.")
app.add_typer(gpnvm_app, name="gpnvm")

_opts: dict = {}


@app.callback()
def _main(
    transport: str = typer.Option("swd", "--transport", "-t",
        help="swd, or jtag (the Atmel-ICE firmware is flaky on JTAG)."),
    speed: int = typer.Option(400, "--speed", "-s", help="Adapter clock in kHz."),
    serial: Optional[str] = typer.Option(None, "--serial",
        help="Atmel-ICE serial number, if more than one is attached."),
    verbose: bool = typer.Option(False, "--verbose", "-v", help="Echo the raw OpenOCD log."),
):
    _opts.update(transport=transport, speed=speed, serial=serial, verbose=verbose)


def connect(attempts: int = 3) -> Session:
    """Open a session and prove it works by halting the core.

    Attaching to a SAM4E that is executing ROM (SAM-BA) code occasionally drops
    the CMSIS-DAP link on the first transaction, so retry before giving up.
    """
    last = None
    for n in range(1, attempts + 1):
        s = None
        try:
            s = Session(**_opts)
            s.halt()
            return s
        except OpenOCDError as e:
            last = e
            if s is not None:
                s.close()
            if n < attempts:
                console.print(f"[yellow]retrying[/] link attempt {n} failed; "
                              f"reconnecting ({n + 1}/{attempts})...")
                time.sleep(0.5)
    err.print(Panel(str(last), title="[red]cannot reach the target", border_style="red"))
    raise typer.Exit(1)


def guard(fn):
    """Report OpenOCD failures as a tidy message rather than a traceback."""
    @functools.wraps(fn)
    def wrapper(*a, **kw):
        try:
            return fn(*a, **kw)
        except OpenOCDError as e:
            err.print(Panel(str(e), title="[red]error", border_style="red"))
            raise typer.Exit(1)
    return wrapper


def _print_gpnvm(bits: dict[int, int]) -> None:
    t = Table(box=None, padding=(0, 2))
    t.add_column("bit", style="cyan")
    t.add_column("name")
    t.add_column("value")
    t.add_column("meaning", style="dim")
    for n, v in sorted(bits.items()):
        if n == 1:
            meaning = "boot from FLASH (application)" if v else "boot from ROM (SAM-BA)"
        elif n == 0:
            meaning = "[red]LOCKED -- debug access disabled[/red]" if v else "unlocked"
        else:
            meaning = GPNVM_BITS[n][1]
        t.add_row(str(n), GPNVM_BITS[n][0], f"[bold]{v}[/bold]", meaning)
    console.print(Panel(t, title="[bold]GPNVM", border_style="blue"))


@app.command()
@guard
def info():
    """Show chip ID, flash geometry, GPNVM bits and core state."""
    with connect() as s:
        c = ChipInfo(s)
        bits = read_gpnvm(s)
        state = s.value("$_TARGETNAME curstate")

        t = Table(show_header=False, box=None, padding=(0, 2))
        t.add_column(style="cyan", justify="right")
        t.add_column()
        t.add_row("Part", f"[bold]{c.name}[/bold]")
        t.add_row("CHIPID", f"CIDR={c.cidr:#010x}  EXID={c.exid:#010x}")
        t.add_row("Flash", f"{c.fl_size // 1024} KB, {FLASH_BASE:#010x}-{c.flash_end - 1:#010x} "
                           f"({c.pages} pages x {c.page_size} B)")
        t.add_row("Lock regions", f"{c.nb_lock} x {c.lock_size // 1024} KB")
        t.add_row("SRAM", f"{c.sram_kb} KB at {SRAM_BASE:#010x}")
        t.add_row("Core", f"Cortex-M4, {state}")
        t.add_row("Link", f"{_opts['transport'].upper()} @ {_opts['speed']} kHz via Atmel-ICE")
        console.print(Panel(t, title="[bold]SAM4E", border_style="green"))

        if c.cidr_flash_kb != c.fl_size // 1024:
            console.print(f"[yellow]note[/] CHIPID claims {c.cidr_flash_kb} KB of flash but the "
                          f"EEFC descriptor reports {c.fl_size // 1024} KB; trusting the EEFC.")
        _print_gpnvm(bits)


@app.command("images")
def images_cmd():
    """List the named images defined in sam4e.toml."""
    imgs = load_images()
    if not imgs:
        console.print(f"[yellow]no images defined in {IMAGES_TOML}")
        return
    for name, spec in imgs.items():
        t = Table(show_header=False, box=None, padding=(0, 2))
        t.add_column(style="green", no_wrap=True)
        t.add_column(overflow="fold")
        for p in spec["parts"]:
            f = Path(p["file"])
            mark = "" if f.exists() else "  [red](missing)[/red]"
            where = f"{p['addr']:#010x}" if "addr" in p else "in image"
            t.add_row(where, f"{f}{mark}")
        desc = spec.get("description")
        console.print(Panel(t, title=f"[bold cyan]{name}",
                            subtitle=f"[dim]{desc}[/dim]" if desc else None,
                            title_align="left", subtitle_align="left",
                            border_style="green"))


@gpnvm_app.command("show")
@guard
def gpnvm_show():
    """Print the current GPNVM bits."""
    with connect() as s:
        _print_gpnvm(read_gpnvm(s))


@gpnvm_app.command("set")
@guard
def gpnvm_set(bit: int = typer.Argument(..., help="GPNVM bit (1 = BOOT_MODE)."),
              force: bool = typer.Option(False, "--force", help="Required for bit 0.")):
    """Set a GPNVM bit to 1."""
    _gpnvm_write(bit, 1, force)


@gpnvm_app.command("clear")
@guard
def gpnvm_clear(bit: int = typer.Argument(..., help="GPNVM bit (1 = BOOT_MODE)."),
                force: bool = typer.Option(False, "--force", help="Required for bit 0.")):
    """Clear a GPNVM bit to 0."""
    _gpnvm_write(bit, 0, force)


def _gpnvm_write(bit: int, value: int, force: bool) -> None:
    if bit not in GPNVM_BITS:
        err.print(f"[red]GPNVM bit {bit} is not defined on the SAM4E "
                  f"(valid: {', '.join(map(str, GPNVM_BITS))}).")
        raise typer.Exit(2)
    if bit == 0 and value == 1 and not force:
        err.print(Panel(
            "Setting GPNVM bit 0 enables the [bold]security bit[/bold]. That permanently\n"
            "disables JTAG/SWD; the only way back is asserting the ERASE pin, which\n"
            "wipes the whole flash. Re-run with [bold]--force[/bold] if you mean it.",
            title="[red]refusing to lock the chip", border_style="red"))
        raise typer.Exit(2)

    with connect() as s:
        s.halt()
        before = read_gpnvm(s)
        s.run(f"at91sam4 gpnvm {'set' if value else 'clear'} {bit}")
        time.sleep(0.05)
        after = read_gpnvm(s)
        if after[bit] != value:
            err.print(f"[red]GPNVM{bit} is still {after[bit]}, expected {value}.")
            raise typer.Exit(1)
        console.print(f"[green]ok[/] GPNVM{bit} ({GPNVM_BITS[bit][0]}): "
                      f"{before[bit]} -> {after[bit]}")
        _print_gpnvm(after)
        if bit == 1:
            console.print("[dim]Takes effect on the next reset -- run [bold]sam4e reset[/bold].[/dim]")


@app.command()
@guard
def boot(mode: str = typer.Argument(..., help="'flash' (GPNVM1=1) or 'samba' (GPNVM1=0)."),
         reset_now: bool = typer.Option(True, "--reset/--no-reset", help="Reset afterwards.")):
    """Pick the boot source -- a friendly wrapper around GPNVM bit 1."""
    m = mode.lower()
    if m in ("flash", "app"):
        want = 1
    elif m in ("samba", "sam-ba", "rom", "bootloader"):
        want = 0
    else:
        err.print("[red]mode must be 'flash' or 'samba'.")
        raise typer.Exit(2)

    with connect() as s:
        s.halt()
        if read_gpnvm(s)[1] == want:
            console.print(f"[green]ok[/] already booting from "
                          f"{'FLASH' if want else 'ROM (SAM-BA)'}.")
        else:
            s.run(f"at91sam4 gpnvm {'set' if want else 'clear'} 1")
            time.sleep(0.05)
            if read_gpnvm(s)[1] != want:
                err.print("[red]GPNVM1 did not change.")
                raise typer.Exit(1)
            console.print(f"[green]ok[/] boot source -> "
                          f"{'FLASH (application)' if want else 'ROM (SAM-BA bootloader)'}")
        if reset_now:
            s.run("reset run")
            console.print("[green]ok[/] device reset.")


@app.command()
@guard
def erase(start: Optional[str] = typer.Option(None, "--start", help="Start address."),
          end: Optional[str] = typer.Option(None, "--end", help="End address, exclusive."),
          yes: bool = typer.Option(False, "--yes", "-y", help="Skip the confirmation.")):
    """Erase flash -- the whole chip by default, or a [--start,--end) range."""
    with connect() as s:
        c = ChipInfo(s)
        whole = start is None and end is None
        if whole:
            what = f"the ENTIRE {c.fl_size // 1024} KB flash"
        else:
            a = as_flash_addr(start) if start else FLASH_BASE
            b = as_flash_addr(end) if end else c.flash_end
            if not (FLASH_BASE <= a < b <= c.flash_end):
                err.print(f"[red]{a:#010x}-{b:#010x} is outside flash "
                          f"({FLASH_BASE:#010x}-{c.flash_end:#010x}).")
                raise typer.Exit(2)
            what = f"flash {a:#010x}-{b - 1:#010x} ({(b - a) // 1024} KB)"

        if not yes and not typer.confirm(f"Erase {what}?"):
            raise typer.Exit(1)

        s.halt()
        with status(f"erasing {what}..."):
            if whole:
                s.run("flash erase_sector 0 0 last")
            else:
                s.run(f"flash erase_address {a:#x} {b - a:#x}")
        console.print(f"[green]ok[/] erased {what}.")


def _write_part(s: Session, c: ChipInfo, path: Path, addr: Optional[int],
                verify: bool) -> None:
    is_bin = _image_kind(path) == "bin"
    loc = ""
    if addr is not None:
        if not (FLASH_BASE <= addr < c.flash_end):
            raise OpenOCDError(f"{addr:#010x} is outside flash "
                               f"({FLASH_BASE:#010x}-{c.flash_end:#010x}).")
        if is_bin and addr + path.stat().st_size > c.flash_end:
            raise OpenOCDError(f"{path.name} ({path.stat().st_size} B) at {addr:#010x} "
                               f"runs past the end of flash ({c.flash_end:#010x}).")
        loc = f" {addr:#x}" + (" bin" if is_bin else "")

    p = path.resolve().as_posix()
    label = f"{path.name}" + (f" @ {addr:#010x}" if addr is not None else "")
    with status(f"programming {label}..."):
        out = s.run(f'flash write_image erase unlock "{p}"{loc}')
    m = re.search(r"wrote (\d+) bytes.*?in ([\d.]+)s.*?([\d.]+ \w+/s)", out)
    extra = f"  [dim]{m.group(1)} B in {m.group(2)}s, {m.group(3)}[/dim]" if m else ""
    console.print(f"[green]ok[/] wrote [bold]{label}[/bold]{extra}")

    if verify:
        with status(f"verifying {path.name}..."):
            s.run(f'verify_image "{p}"' + (f" {addr:#x}" if addr is not None else ""))
        console.print(f"[green]ok[/] verified {path.name}")


@app.command("flash")
@guard
def flash_cmd(
    target: str = typer.Argument(...,
        help="A named image from sam4e.toml, or a .bin/.elf/.axf/.hex/.ihex/.s19/.srec file."),
    addr: Optional[str] = typer.Option(None, "--addr", "-a",
        help="Load address; required for a raw .bin, rejected for self-addressed files."),
    verify: bool = typer.Option(True, "--verify/--no-verify"),
    run_after: bool = typer.Option(True, "--run/--no-run", help="Reset and run when done."),
):
    """Program flash from a named image or a file."""
    imgs = load_images()
    if target in imgs:
        if addr is not None:
            err.print("[red]--addr does not apply to a named image; "
                      "edit sam4e.toml instead.")
            raise typer.Exit(2)
        raw_parts = [(Path(p["file"]), p.get("addr")) for p in imgs[target]["parts"]]
    else:
        f = Path(target)
        if not f.is_file():
            err.print(f"[red]'{target}' is neither a named image "
                      f"({', '.join(imgs) or 'none defined'}) nor an existing file.")
            raise typer.Exit(2)
        raw_parts = [(f, addr)]

    try:
        parts = [(f, as_flash_addr(a) if a is not None else None) for f, a in raw_parts]
        for f, a in parts:
            if not f.is_file():
                raise ValueError(f"missing file: {f}")
            _validate_image_part(f, a)
    except (TypeError, ValueError) as e:
        err.print(f"[red]{e}")
        raise typer.Exit(2)

    with connect() as s:
        c = ChipInfo(s)
        s.halt()
        for f, a in parts:
            _write_part(s, c, f, a, verify)
        if run_after:
            s.run("reset run")
            console.print("[green]ok[/] reset, running.")


@app.command("read")
@guard
def read_cmd(addr: str = typer.Argument(..., help="Absolute start address."),
             length: str = typer.Argument(..., help="Byte count, e.g. 256, 4k, 0x1000."),
             out: Optional[Path] = typer.Option(None, "--out", "-o",
                 help="Dump to a .bin file instead of hexdumping.")):
    """Read flash or memory -- hexdump to the terminal, or dump to a file."""
    try:
        a, n = parse_int(addr), parse_int(length)
        if not 0 <= a <= 0xFFFFFFFF:
            raise ValueError("address must be between 0 and 0xffffffff")
        if n <= 0:
            raise ValueError("length must be positive")
    except ValueError as e:
        err.print(f"[red]{e}")
        raise typer.Exit(2)
    with connect() as s:
        s.halt()
        if out:
            with status(f"reading {n} bytes from {a:#010x}..."):
                s.run(f'dump_image "{out.resolve().as_posix()}" {a:#x} {n:#x}')
            console.print(f"[green]ok[/] wrote {n} bytes to [bold]{out}[/bold]")
            return
        words = s.read32(a, (n + 3) // 4)
        data = b"".join(w.to_bytes(4, "little") for w in words)[:n]
        for off in range(0, len(data), 16):
            row = data[off:off + 16]
            hexs = " ".join(f"{b:02x}" for b in row).ljust(47)
            text = "".join(chr(b) if 32 <= b < 127 else "." for b in row)
            console.print(f"[cyan]{a + off:08x}[/]  {hexs}  |{text}|")


@app.command()
@guard
def reset(halt: bool = typer.Option(False, "--halt", help="Stop at the reset vector.")):
    """Reboot the device."""
    with connect() as s:
        s.run("reset halt" if halt else "reset run")
        console.print(f"[green]ok[/] reset ({'halted' if halt else 'running'}).")


@app.command()
@guard
def halt():
    """Halt the core."""
    with connect() as s:
        s.halt()
        console.print(f"[green]ok[/] core {s.value('$_TARGETNAME curstate')}.")


@app.command()
@guard
def resume():
    """Resume a halted core."""
    with connect() as s:
        s.run("resume")
        console.print("[green]ok[/] running.")


@app.command()
def gdb(port: int = typer.Option(3333, "--port", "-p")):
    """Run a persistent OpenOCD gdb server (Ctrl-C to stop)."""
    console.print(Panel(
        f"gdb server on [bold]localhost:{port}[/bold]\n\n"
        f"arm-none-eabi-gdb -ex 'target extended-remote :{port}' your.elf",
        border_style="green"))
    subprocess.run([shutil.which("openocd"),
                    "-c", f"set TRANSPORT {_opts['transport']}",
                    "-c", f"set SPEED {_opts['speed']}",
                    "-c", f"gdb_port {port}",
                    "-f", str(CFG)])


@app.command()
@guard
def raw(command: list[str] = typer.Argument(..., help="e.g. sam4e raw flash info 0")):
    """Escape hatch: run an arbitrary OpenOCD command."""
    with connect() as s:
        console.print(s.run(" ".join(command)))


if __name__ == "__main__":
    app()
