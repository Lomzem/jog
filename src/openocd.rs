use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{AppError, AppResult};

const RPC_EOM: u8 = 0x1a;
const MARK: &str = "---SAM4E-OUT---";

pub(crate) struct OpenOcdOptions {
    pub(crate) executable: PathBuf,
    pub(crate) transport: String,
    pub(crate) speed: u32,
    pub(crate) serial: Option<String>,
    pub(crate) verbose: bool,
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

    fn restore(message: impl Into<String>) -> Self {
        Self::fatal(format!(
            "could not restore the target entry state; the target can remain halted: {}",
            message.into()
        ))
    }
}

pub(crate) struct Session {
    child: Child,
    socket: Option<TcpStream>,
    log: Arc<Mutex<VecDeque<String>>>,
    entry_state: String,
    resume_on_drop: bool,
}

pub(crate) fn connect(options: &OpenOcdOptions) -> AppResult<Session> {
    connect_with_policy(options, false)
}

pub(crate) fn connect_read_only(options: &OpenOcdOptions) -> AppResult<Session> {
    connect_with_policy(options, true)
}

fn connect_with_policy(options: &OpenOcdOptions, restore_entry_state: bool) -> AppResult<Session> {
    let mut last = None;
    for attempt in 1..=3 {
        match Session::start(options).and_then(|mut session| {
            // The first background poll may not have run when Tcl connects.
            session.run_inner("poll")?;
            let state = session.value_inner("$_TARGETNAME curstate")?;
            if state != "running" && state != "halted" {
                return Err(SessionError::retryable(format!(
                    "cannot determine target entry state: {state}"
                )));
            }
            session.resume_on_drop = restore_entry_state && state == "running";
            session.entry_state = state;
            if let Err(error) = session.run_inner("halt") {
                session.restore_entry_state_inner()?;
                return Err(error);
            }
            Ok(session)
        }) {
            Ok(session) => return Ok(session),
            Err(error) => {
                let retryable = error.retryable;
                let delay = retry_delay(&error.message, attempt);
                last = Some(error.message);
                if !retryable || attempt == 3 {
                    break;
                }
                eprintln!(
                    "retrying: link attempt {attempt} failed; reconnecting in {} s ({}/3)",
                    delay.as_secs_f32(),
                    attempt + 1
                );
                thread::sleep(delay);
            }
        }
    }
    Err(AppError::Runtime(format!(
        "cannot reach the target: {}\nCheck target power, cables, --serial, and the 400 kHz default. Use --verbose for the OpenOCD log.",
        last.unwrap_or_else(|| "unknown OpenOCD failure".into())
    )))
}

fn retry_delay(message: &str, attempt: u32) -> Duration {
    // The ROM can stall debug access for about 18 seconds after reset on
    // the tested target. Keep the first retry quick for a dropped packet.
    if attempt >= 2
        && [
            "stalled AP operation",
            "DAP transaction stalled",
            "WAIT recovery",
        ]
        .iter()
        .any(|detail| message.contains(detail))
    {
        Duration::from_secs(20)
    } else {
        Duration::from_millis(500)
    }
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
                SessionError::fatal(openocd_start_error(&options.executable, error).to_string())
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
            entry_state: String::new(),
            resume_on_drop: false,
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
                return Err(SessionError::retryable(format!(
                    "OpenOCD exited during startup ({status}){}",
                    self.log_tail(15)
                )));
            }
            match TcpStream::connect_timeout(
                &SocketAddr::from(([127, 0, 0, 1], port)),
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
        parse_run_response(&self.rpc(&rpc_wrapper(tcl))?, tcl)
    }

    pub(crate) fn run(&mut self, tcl: &str) -> AppResult<String> {
        self.run_inner(tcl)
            .map_err(|error| AppError::Runtime(error.message))
    }

    fn value_inner(&mut self, expression: &str) -> Result<String, SessionError> {
        self.rpc(expression).map(|value| value.trim().to_owned())
    }

    pub(crate) fn value(&mut self, expression: &str) -> AppResult<String> {
        self.value_inner(expression)
            .map_err(|error| AppError::Runtime(error.message))
    }

    pub(crate) fn entry_state(&self) -> &str {
        &self.entry_state
    }

    fn restore_entry_state_inner(&mut self) -> Result<(), SessionError> {
        if !self.resume_on_drop {
            return Ok(());
        }
        self.run_inner("resume")
            .map_err(|error| SessionError::restore(error.message))?;
        let state = self
            .value_inner("$_TARGETNAME curstate")
            .map_err(|error| SessionError::restore(error.message))?;
        if state != "running" {
            return Err(SessionError::restore(format!(
                "expected running, got {state}"
            )));
        }
        self.resume_on_drop = false;
        Ok(())
    }

    pub(crate) fn restore_entry_state(&mut self) -> AppResult<()> {
        self.restore_entry_state_inner()
            .map_err(|error| AppError::Runtime(error.message))
    }

    pub(crate) fn read32(&mut self, address: u32, count: usize) -> AppResult<Vec<u32>> {
        self.read_memory(address, 32, count)
    }

    pub(crate) fn read8(&mut self, address: u32, count: usize) -> AppResult<Vec<u8>> {
        self.read_memory(address, 8, count)?
            .into_iter()
            .map(|word| {
                u8::try_from(word)
                    .map_err(|_| AppError::Runtime(format!("invalid byte from OpenOCD: {word}")))
            })
            .collect()
    }

    fn read_memory(&mut self, address: u32, width: u8, count: usize) -> AppResult<Vec<u32>> {
        let words = self
            .value(&format!("read_memory {address:#x} {width} {count}"))?
            .split_whitespace()
            .map(|word| {
                let digits = word.strip_prefix("0x").unwrap_or(word);
                u32::from_str_radix(digits, if word.starts_with("0x") { 16 } else { 10 })
                    .map_err(|_| AppError::Runtime(format!("invalid word from OpenOCD: {word}")))
            })
            .collect::<AppResult<Vec<_>>>()?;
        if words.len() != count {
            return Err(AppError::Runtime(format!(
                "incomplete memory data from OpenOCD: expected {count} values, got {}",
                words.len()
            )));
        }
        Ok(words)
    }

    pub(crate) fn write32(&mut self, address: u32, words: &[u32]) -> AppResult<()> {
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

fn parse_run_response(raw: &str, tcl: &str) -> Result<String, SessionError> {
    let (result, output) = raw
        .split_once(&format!("\n{MARK}\n"))
        .ok_or_else(|| SessionError::retryable(format!("invalid response from OpenOCD: {raw}")))?;
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
        if self.resume_on_drop {
            let _ = self.restore_entry_state_inner();
        }
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

pub(crate) fn openocd_args(
    options: &OpenOcdOptions,
    tcl_port: Option<u16>,
    gdb_port: Option<u16>,
) -> Vec<OsString> {
    let mut commands = vec![
        "adapter driver cmsis-dap".to_owned(),
        // OpenOCD releases use different names for USB adapter selection.
        "if {[catch {adapter usb vid_pid 0x03eb 0x2141}]} { if {[catch {cmsis-dap vid_pid 0x03eb 0x2141}]} { cmsis_dap_vid_pid 0x03eb 0x2141 } }".to_owned(),
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
        // The common Atmel-ICE header has no SRST, so reset uses SYSRESETREQ.
        "reset_config none".to_owned(),
        format!("adapter speed {}", options.speed),
        "bindto 127.0.0.1".to_owned(),
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

pub(crate) fn openocd_start_error(executable: &Path, error: io::Error) -> AppError {
    AppError::Runtime(format!(
        "could not start OpenOCD '{}': {error}; install OpenOCD or use --openocd PATH",
        executable.display()
    ))
}

fn free_port() -> io::Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

fn rpc_wrapper(tcl: &str) -> String {
    let command = tcl_literal(tcl);
    format!(
        "set __cmd {command}\nset __rc [catch [list capture $__cmd] __out]\nappend __rc \"\\n{MARK}\\n\" $__out"
    )
}

pub(crate) fn tcl_literal(value: &str) -> String {
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

pub(crate) fn path_string(path: &Path) -> AppResult<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| AppError::Usage(format!("path is not valid Unicode: {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_rom_startup_only_after_repeated_debug_stalls() {
        assert_eq!(
            retry_delay("stalled AP operation", 1),
            Duration::from_millis(500)
        );
        assert_eq!(
            retry_delay("stalled AP operation", 2),
            Duration::from_secs(20)
        );
        assert_eq!(
            retry_delay("Timeout during WAIT recovery", 2),
            Duration::from_secs(20)
        );
        assert_eq!(
            retry_delay("probe not found", 2),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn quotes_tcl_literals_without_rpc_end_bytes() {
        assert_eq!(
            tcl_literal("C:\\a $b [x] \"q\"\r\n\t"),
            r#""C:\\a \$b \[x\] \"q\"\r\n\t""#
        );
        let wrapper = rpc_wrapper("echo before\u{001a}after");
        assert!(!wrapper.as_bytes().contains(&RPC_EOM));
        assert!(wrapper.contains("\\x1a"));
    }

    #[test]
    fn restores_running_entry_state_with_a_checked_response() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let server_thread = thread::spawn(move || {
            let mut request = read_request(&mut server);
            assert!(String::from_utf8_lossy(&request).contains("resume"));
            server
                .write_all(format!("0\n{MARK}\n\x1a").as_bytes())
                .unwrap();

            request = read_request(&mut server);
            assert_eq!(&request[..request.len() - 1], b"$_TARGETNAME curstate");
            server.write_all(b"running\x1a").unwrap();
        });
        let child = test_child();
        let mut session = Session {
            child,
            socket: Some(client),
            log: Arc::new(Mutex::new(VecDeque::new())),
            entry_state: "running".into(),
            resume_on_drop: true,
        };

        session.restore_entry_state().unwrap();
        assert!(!session.resume_on_drop);
        server_thread.join().unwrap();
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        while !request.ends_with(&[RPC_EOM]) {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        request
    }

    fn memory_session(
        expected_request: &'static str,
        response: &'static str,
    ) -> (Session, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = thread::spawn(move || {
            let request = read_request(&mut server);
            assert_eq!(&request[..request.len() - 1], expected_request.as_bytes());
            server.write_all(response.as_bytes()).unwrap();
            server.write_all(&[RPC_EOM]).unwrap();
        });
        let session = Session {
            child: test_child(),
            socket: Some(client),
            log: Arc::new(Mutex::new(VecDeque::new())),
            entry_state: "halted".into(),
            resume_on_drop: false,
        };
        (session, worker)
    }

    #[test]
    fn reads_exact_bytes_at_an_unaligned_address() {
        let (mut session, worker) = memory_session("read_memory 0x400001 8 3", "0x00 127 0xff");
        let bytes = session.read8(0x400001, 3).unwrap();
        worker.join().unwrap();
        assert_eq!(bytes, [0, 127, 255]);
    }

    #[test]
    fn rejects_incomplete_byte_data() {
        let (mut session, worker) = memory_session("read_memory 0x400001 8 3", "0x01 2");
        let error = session.read8(0x400001, 3).unwrap_err();
        worker.join().unwrap();
        assert!(error.to_string().contains("expected 3 values, got 2"));
    }

    #[test]
    fn rejects_values_outside_a_byte() {
        let (mut session, worker) = memory_session("read_memory 0x400001 8 1", "0x100");
        let error = session.read8(0x400001, 1).unwrap_err();
        worker.join().unwrap();
        assert!(error.to_string().contains("invalid byte from OpenOCD: 256"));
    }

    #[test]
    fn rejects_incomplete_word_data() {
        let (mut session, worker) = memory_session("read_memory 0x400000 32 2", "0x12345678");
        let error = session.read32(0x400000, 2).unwrap_err();
        worker.join().unwrap();
        assert!(error.to_string().contains("expected 2 values, got 1"));
    }

    #[cfg(unix)]
    fn test_child() -> Child {
        Command::new("true").spawn().unwrap()
    }

    #[cfg(windows)]
    fn test_child() -> Child {
        Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap()
    }

    #[test]
    fn does_not_retry_a_failed_state_restore() {
        let error = SessionError::restore("lost response");
        assert!(!error.retryable);
        assert!(error.message.contains("target can remain halted"));
    }

    #[test]
    fn parses_captured_openocd_responses() {
        assert_eq!(
            parse_run_response(&format!("0\n{MARK}\n output \n"), "halt")
                .unwrap_or_else(|error| panic!("{}", error.message)),
            "output"
        );
        assert!(parse_run_response(&format!("1\n{MARK}\nfailed"), "halt").is_err());
        assert!(parse_run_response("unframed", "halt").is_err());
    }

    #[test]
    fn builds_embedded_openocd_config() {
        let options = OpenOcdOptions {
            executable: "openocd".into(),
            transport: "swd".into(),
            speed: 400,
            serial: Some("A $B".into()),
            verbose: false,
        };
        let text = openocd_args(&options, Some(1234), None)
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("source [find target/at91sam4XXX.cfg]"));
        assert!(text.contains("adapter serial \"A \\$B\""));
        assert!(text.contains("bindto 127.0.0.1"));
        assert!(text.contains("tcl_port 1234"));
        assert!(text.contains("gdb_port disabled"));
        assert!(!text.contains(" -f "));
    }
}
