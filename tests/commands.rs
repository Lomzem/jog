use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jog-commands-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("jog.toml"), "[images]\n").unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_jog"));
        command
            .current_dir(&self.0)
            .arg("--openocd")
            .arg(self.0.join("missing-openocd"))
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_error(output: Output, code: i32, message: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(code), "{stderr}");
    assert!(
        stderr.contains(message),
        "Expected {message:?}, got {stderr:?}"
    );
}

#[test]
fn rejects_invalid_commands_before_starting_openocd() {
    let workspace = Workspace::new();
    let cases: &[(&[&str], &str)] = &[
        (&["--speed", "0", "info"], "invalid value"),
        (&["gdb", "--port", "0"], "invalid value"),
        (&["gpnvm", "set", "0"], "--force"),
        (&["gpnvm", "set", "2"], "not defined"),
        (&["gpnvm", "clear", "2"], "not defined"),
        (&["read", "0x100000000", "4"], "address is too large"),
        (&["read", "0xffffffff", "2"], "32-bit address space"),
        (&["read", "0x400000", "0"], "length must be positive"),
        (
            &["erase", "--start", "0x100", "--end", "0x100", "--yes"],
            "must be below",
        ),
        (&["erase"], "--yes"),
        (
            &["flash", "missing-image"],
            "not an existing file or named image",
        ),
    ];
    for (args, message) in cases {
        assert_error(workspace.run(args), 2, message);
    }
}

#[test]
fn help_and_version_do_not_need_openocd() {
    let workspace = Workspace::new();
    for args in [["--help"], ["--version"]] {
        let output = workspace.run(&args);
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("jog"));
    }
}

#[test]
fn lists_split_image_from_config_relative_paths() {
    let workspace = Workspace::new();
    let config_dir = workspace.0.join("config");
    fs::create_dir(&config_dir).unwrap();
    fs::write(config_dir.join("pt1.bin"), [1, 2, 3, 4]).unwrap();
    fs::write(
        config_dir.join("images.toml"),
        "[images.bootloader]\ndescription = 'Two parts'\nparts = [{ file = 'pt1.bin', addr = 0x400000 }, { file = 'pt2.bin', addr = 0x47a000 }]\n",
    ).unwrap();
    let output = workspace.run(&["--config", "config/images.toml", "images"]);
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("bootloader - Two parts"), "{stdout}");
    assert!(stdout.contains("0x00400000"), "{stdout}");
    assert!(stdout.contains("0x0047a000"), "{stdout}");
    let first = stdout
        .lines()
        .find(|line| line.contains("pt1.bin"))
        .unwrap();
    let second = stdout
        .lines()
        .find(|line| line.contains("pt2.bin"))
        .unwrap();
    assert!(!first.contains("missing"), "{stdout}");
    assert!(second.contains("missing"), "{stdout}");
    assert_error(
        workspace.run(&["--config", "config/images.toml", "flash", "bootloader"]),
        2,
        "image file does not exist",
    );
}

#[test]
fn config_directory_prints_path_without_loading_config_or_starting_openocd() {
    let workspace = Workspace::new();
    fs::write(workspace.0.join("jog.toml"), "invalid TOML").unwrap();
    let config_home = workspace.0.join("user-config");
    let output = workspace
        .command()
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("--config-dir")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        config_home.join("jog").to_str().unwrap()
    );
    assert!(output.stderr.is_empty());
    assert!(!config_home.exists());
}

#[test]
fn config_init_creates_user_config_with_inactive_examples() {
    let workspace = Workspace::new();
    fs::write(workspace.0.join("jog.toml"), "invalid TOML").unwrap();
    let config_home = workspace.0.join("user-config");
    let config_path = config_home.join("jog/jog.toml");
    let output = workspace
        .command()
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("--config-init")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(config_path.to_str().unwrap()),
        "{output:?}"
    );
    let template = fs::read_to_string(&config_path).unwrap();
    assert!(template.contains("[images."), "{template}");
    assert!(
        template
            .lines()
            .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#')),
        "{template}"
    );
    let output = workspace.run(&["--config", config_path.to_str().unwrap(), "images"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("Config: {}\nNo images are defined.", config_path.display())
    );
}

#[test]
fn config_init_accepts_explicit_paths_and_preserves_existing_files() {
    let workspace = Workspace::new();
    for path in ["new-config.toml", "nested/config/jog.toml"] {
        let output = workspace.run(&["--config", path, "--config-init"]);
        assert!(output.status.success(), "{output:?}");
        assert!(workspace.0.join(path).is_file());

        let original = "# Existing configuration must be preserved.\n[images]\n";
        fs::write(workspace.0.join(path), original).unwrap();
        let output = workspace.run(&["--config", path, "--config-init"]);
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(
            fs::read_to_string(workspace.0.join(path)).unwrap(),
            original
        );
    }
}

#[test]
fn config_init_conflicts_with_config_directory() {
    let workspace = Workspace::new();
    let config_home = workspace.0.join("user-config");
    let output = workspace
        .command()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["--config-init", "--config-dir"])
        .output()
        .unwrap();
    assert_error(output, 2, "cannot be used with");
    assert!(!config_home.exists());
}

#[test]
fn rejects_missing_explicit_config_and_unknown_fields() {
    let workspace = Workspace::new();
    assert_error(
        workspace.run(&["--config", "absent.toml", "images"]),
        2,
        "configuration file does not exist",
    );
    fs::write(workspace.0.join("jog.toml"), "imagse = {}\n").unwrap();
    assert_error(workspace.run(&["images"]), 2, "unknown field");
}

#[test]
fn missing_openocd_reports_runtime_error() {
    let workspace = Workspace::new();
    assert_error(workspace.run(&["info"]), 1, "could not start OpenOCD");
}

#[cfg(unix)]
#[test]
fn failed_openocd_startup_reports_log_and_stops_after_three_attempts() {
    use std::os::unix::fs::PermissionsExt;

    let workspace = Workspace::new();
    let fake = workspace.0.join("fake-openocd");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf 'attempt\\n' >> attempts.log\necho 'test probe startup failed' >&2\nexit 7\n",
    ).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_jog"))
        .current_dir(&workspace.0)
        .arg("--openocd")
        .arg(fake)
        .arg("info")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_error(output, 1, "test probe startup failed");
    assert_eq!(
        fs::read_to_string(workspace.0.join("attempts.log"))
            .unwrap()
            .lines()
            .count(),
        3
    );
}
