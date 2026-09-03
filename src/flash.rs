use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::openocd::{Session, path_string, tcl_literal};
use crate::target::{FLASH_BASE, normalize_flash_addr};
use crate::{AppError, AppResult};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    images: BTreeMap<String, Image>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Image {
    pub(crate) description: Option<String>,
    pub(crate) parts: Vec<ImagePart>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImagePart {
    pub(crate) file: PathBuf,
    pub(crate) addr: Option<u64>,
}

pub(crate) struct LoadedConfig {
    pub(crate) path: Option<PathBuf>,
    pub(crate) images: BTreeMap<String, Image>,
}

pub(crate) struct FlashPart {
    pub(crate) file: PathBuf,
    pub(crate) addr: Option<u32>,
}

pub(crate) enum FlashPlan {
    Single,
    MultiBin(Vec<Range<u32>>),
}

pub(crate) fn load_config(explicit: Option<&Path>) -> AppResult<LoadedConfig> {
    let executable = env::current_exe()
        .map_err(|error| AppError::Usage(format!("cannot locate the sam4e executable: {error}")))?;
    let current_dir = env::current_dir().map_err(|error| {
        AppError::Usage(format!("cannot locate the current directory: {error}"))
    })?;
    let candidates = config_candidates(
        explicit,
        &current_dir,
        &executable,
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("HOME"),
        env::var_os("APPDATA"),
        cfg!(windows),
    );
    if explicit.is_some() && !candidates[0].is_file() {
        return Err(AppError::Usage(format!(
            "configuration file does not exist: {}",
            candidates[0].display()
        )));
    }
    let Some(path) = candidates.into_iter().find(|path| path.is_file()) else {
        return Ok(LoadedConfig {
            path: None,
            images: BTreeMap::new(),
        });
    };
    parse_config_file(&path)
}

fn config_candidates(
    explicit: Option<&Path>,
    current_dir: &Path,
    executable: &Path,
    xdg: Option<OsString>,
    home: Option<OsString>,
    appdata: Option<OsString>,
    windows: bool,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path.to_owned()];
    }
    let mut paths = vec![current_dir.join("sam4e.toml")];
    if let Some(parent) = executable.parent() {
        paths.push(parent.join("sam4e.toml"));
    }
    if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(xdg).join("sam4e").join("sam4e.toml"));
    }
    if windows {
        if let Some(appdata) = appdata.filter(|value| !value.is_empty()) {
            paths.push(PathBuf::from(appdata).join("sam4e").join("sam4e.toml"));
        }
    } else if let Some(home) = home.filter(|value| !value.is_empty()) {
        paths.push(
            PathBuf::from(home)
                .join(".config")
                .join("sam4e")
                .join("sam4e.toml"),
        );
    }
    paths.dedup();
    paths
}

fn parse_config_file(path: &Path) -> AppResult<LoadedConfig> {
    let text = fs::read_to_string(path)
        .map_err(|error| AppError::Usage(format!("cannot read {}: {error}", path.display())))?;
    let mut config: Config = toml::from_str(&text).map_err(|error| {
        let mut message = format!("cannot parse {}: {error}", path.display());
        if error.to_string().to_ascii_lowercase().contains("escape") {
            message.push_str("; use single quotes for Windows paths");
        }
        AppError::Usage(message)
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    for (name, image) in &mut config.images {
        if name.trim().is_empty() {
            return Err(AppError::Usage("image names must not be empty".into()));
        }
        if image.parts.is_empty() {
            return Err(AppError::Usage(format!("image '{name}' has no parts")));
        }
        for part in &mut image.parts {
            if part.file.as_os_str().is_empty() {
                return Err(AppError::Usage(format!(
                    "an image part in '{name}' has no file"
                )));
            }
            validate_image_address(&part.file, part.addr, &format!("image '{name}'"))?;
            if let Some(address) = part.addr {
                normalize_flash_addr(address)?;
            }
            if part.file.is_relative() {
                part.file = base.join(&part.file);
            }
        }
    }
    Ok(LoadedConfig {
        path: Some(path.to_owned()),
        images: config.images,
    })
}

pub(crate) fn prepare_flash_parts(
    target: &str,
    addr: Option<u32>,
    config: &LoadedConfig,
) -> AppResult<Vec<FlashPart>> {
    let mut parts = if let Some(image) = config.images.get(target) {
        if addr.is_some() {
            return Err(AppError::Usage(
                "--addr does not apply to a named image; edit sam4e.toml instead".into(),
            ));
        }
        image
            .parts
            .iter()
            .map(|part| {
                Ok(FlashPart {
                    file: part.file.clone(),
                    addr: part.addr.map(normalize_flash_addr).transpose()?,
                })
            })
            .collect::<AppResult<Vec<_>>>()?
    } else {
        let file = PathBuf::from(target);
        if !file.is_file() {
            let names = if config.images.is_empty() {
                "none defined".into()
            } else {
                config.images.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            return Err(AppError::Usage(format!(
                "'{target}' is not an existing file or named image ({names})"
            )));
        }
        validate_image_address(&file, addr.map(u64::from), "image file")?;
        vec![FlashPart { file, addr }]
    };
    for part in &mut parts {
        if !part.file.is_file() {
            return Err(AppError::Usage(format!(
                "image file does not exist: {}",
                part.file.display()
            )));
        }
        part.file = fs::canonicalize(&part.file).map_err(|error| {
            AppError::Usage(format!("cannot resolve {}: {error}", part.file.display()))
        })?;
        path_string(&part.file)?;
    }
    Ok(parts)
}

fn validate_image_format(path: &Path) -> AppResult<()> {
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            AppError::Usage(format!(
                "image file has no supported extension: {}",
                path.display()
            ))
        })?;
    match extension.as_str() {
        "bin" => Ok(()),
        "elf" | "axf" | "hex" | "ihex" | "s19" | "srec" => Err(AppError::Usage(format!(
            "self-addressed image {} cannot be validated before erase; use a raw .bin with an address",
            path.display()
        ))),
        _ => Err(AppError::Usage(format!(
            "unsupported image extension '.{extension}' for {}",
            path.display()
        ))),
    }
}

fn validate_image_address(path: &Path, addr: Option<u64>, context: &str) -> AppResult<()> {
    validate_image_format(path)?;
    if addr.is_none() {
        Err(AppError::Usage(format!(
            "raw .bin in {context} requires an address"
        )))
    } else {
        Ok(())
    }
}

pub(crate) fn plan_flash(parts: &[FlashPart], flash_end: u32) -> AppResult<FlashPlan> {
    if parts.is_empty() {
        return Err(AppError::Usage("image has no parts".into()));
    }
    let ranges = parts
        .iter()
        .map(|part| validate_part_range(part, flash_end))
        .collect::<AppResult<Vec<_>>>()?;
    if parts.len() == 1 {
        return Ok(FlashPlan::Single);
    }
    for (index, left) in ranges.iter().enumerate() {
        for right in &ranges[index + 1..] {
            if left.start < right.end && right.start < left.end {
                return Err(AppError::Usage(format!(
                    "multi-part image ranges overlap: {:#010x}-{:#010x} and {:#010x}-{:#010x}",
                    left.start,
                    left.end - 1,
                    right.start,
                    right.end - 1
                )));
            }
        }
    }
    Ok(FlashPlan::MultiBin(ranges))
}

fn validate_part_range(part: &FlashPart, flash_end: u32) -> AppResult<Range<u32>> {
    let address = part
        .addr
        .ok_or_else(|| AppError::Usage("raw .bin image requires an address".into()))?;
    if !(FLASH_BASE..flash_end).contains(&address) {
        return Err(AppError::Usage(format!(
            "{address:#010x} is outside flash ({FLASH_BASE:#010x}-{flash_end:#010x})"
        )));
    }
    let size = fs::metadata(&part.file)
        .map_err(|error| AppError::Usage(format!("cannot read {}: {error}", part.file.display())))?
        .len();
    if size == 0 {
        return Err(AppError::Usage(format!(
            "raw image is empty: {}",
            part.file.display()
        )));
    }
    let end = u64::from(address) + size;
    if end > u64::from(flash_end) {
        return Err(AppError::Usage(format!(
            "{} ({size} B) at {address:#010x} runs past flash end {flash_end:#010x}",
            part.file.display()
        )));
    }
    Ok(address..end as u32)
}

pub(crate) fn write_part(session: &mut Session, part: &FlashPart, erase: bool) -> AppResult<()> {
    let path = tcl_literal(&path_string(&part.file)?);
    let location = part
        .addr
        .map(|address| format!(" {address:#x} bin"))
        .unwrap_or_default();
    let label = part_label(part);
    println!("Programming {label}...");
    let erase = if erase { " erase" } else { "" };
    let output = session
        .run(&format!("flash write_image{erase} unlock {path}{location}"))
        .map_err(AppError::flash_incomplete)?;
    println!("ok: wrote {label}");
    if !output.is_empty() {
        println!("  {}", output.replace('\n', "\n  "));
    }
    Ok(())
}

pub(crate) fn verify_part(session: &mut Session, part: &FlashPart) -> AppResult<()> {
    let path = tcl_literal(&path_string(&part.file)?);
    let address = part
        .addr
        .map(|address| format!(" {address:#x}"))
        .unwrap_or_default();
    println!("Verifying {}...", part.file.display());
    session
        .run(&format!("verify_image {path}{address}"))
        .map_err(AppError::flash_incomplete)?;
    println!("ok: verified {}", part.file.display());
    Ok(())
}

fn part_label(part: &FlashPart) -> String {
    match part.addr {
        Some(address) => format!("{} @ {address:#010x}", part.file.display()),
        None => part.file.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("sam4e-test-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn enforces_image_address_rules() {
        assert!(validate_image_address(Path::new("x.bin"), None, "test").is_err());
        assert!(
            validate_image_address(Path::new("x.bin"), Some(FLASH_BASE.into()), "test").is_ok()
        );
        assert!(
            validate_image_address(Path::new("x.elf"), Some(FLASH_BASE.into()), "test").is_err()
        );
        assert!(validate_image_address(Path::new("x.elf"), None, "test").is_err());
        assert!(validate_image_format(Path::new("x.txt")).is_err());
    }

    #[test]
    fn discovers_project_then_sidecar_then_platform_config() {
        let paths = config_candidates(
            None,
            Path::new("/project"),
            Path::new("/opt/sam4e/sam4e"),
            Some(OsString::from("/xdg")),
            Some(OsString::from("/home/user")),
            Some(OsString::from("C:\\AppData")),
            false,
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/project/sam4e.toml"),
                PathBuf::from("/opt/sam4e/sam4e.toml"),
                PathBuf::from("/xdg/sam4e/sam4e.toml"),
                PathBuf::from("/home/user/.config/sam4e/sam4e.toml")
            ]
        );
        let explicit = config_candidates(
            Some(Path::new("chosen.toml")),
            Path::new("/project"),
            Path::new("/opt/sam4e/sam4e"),
            None,
            None,
            None,
            false,
        );
        assert_eq!(explicit, vec![PathBuf::from("chosen.toml")]);
    }

    #[test]
    fn uses_xdg_then_windows_appdata() {
        let paths = config_candidates(
            None,
            Path::new("/project"),
            Path::new("/portable/sam4e.exe"),
            Some(OsString::from("/xdg")),
            Some(OsString::from("C:\\Users\\u")),
            Some(OsString::from("C:\\Users\\u\\AppData\\Roaming")),
            true,
        );
        assert_eq!(paths[0], PathBuf::from("/project/sam4e.toml"));
        assert_eq!(paths[1], PathBuf::from("/portable/sam4e.toml"));
        assert_eq!(paths[2], PathBuf::from("/xdg/sam4e/sam4e.toml"));
        assert!(paths[3].ends_with(Path::new("sam4e/sam4e.toml")));
    }

    #[test]
    fn parses_config_and_resolves_relative_files() {
        let dir = temp_dir();
        let path = dir.join("sam4e.toml");
        fs::write(
            &path,
            "[images.app]\nparts = [{ file = 'build/app.bin', addr = 0x400000 }]\n",
        )
        .unwrap();
        let config = parse_config_file(&path).unwrap();
        assert_eq!(
            config.images["app"].parts[0].file,
            dir.join("build/app.bin")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_invalid_config_shape() {
        let dir = temp_dir();
        let path = dir.join("sam4e.toml");
        fs::write(&path, "[images.app]\nparts = []\n").unwrap();
        assert!(parse_config_file(&path).is_err());
        fs::write(
            &path,
            "[images.app]\nparts = [{ file = 'app.elf', addr = 1 }]\n",
        )
        .unwrap();
        assert!(parse_config_file(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn plans_multi_part_flash_before_writes() {
        let dir = temp_dir();
        let first = dir.join("first.bin");
        let second = dir.join("second.bin");
        fs::write(&first, [1, 2, 3, 4]).unwrap();
        fs::write(&second, [5, 6, 7, 8]).unwrap();
        let parts = [
            FlashPart {
                file: first,
                addr: Some(FLASH_BASE),
            },
            FlashPart {
                file: second,
                addr: Some(FLASH_BASE + 8),
            },
        ];
        match plan_flash(&parts, FLASH_BASE + 1024).unwrap() {
            FlashPlan::MultiBin(ranges) => assert_eq!(
                ranges,
                vec![FLASH_BASE..FLASH_BASE + 4, FLASH_BASE + 8..FLASH_BASE + 12]
            ),
            FlashPlan::Single => panic!("expected a multi-part plan"),
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_unsafe_multi_part_flash_plans() {
        let dir = temp_dir();
        let first = dir.join("first.bin");
        let second = dir.join("second.bin");
        fs::write(&first, [0; 8]).unwrap();
        fs::write(&second, [0; 8]).unwrap();
        let overlap = [
            FlashPart {
                file: first,
                addr: Some(FLASH_BASE),
            },
            FlashPart {
                file: second,
                addr: Some(FLASH_BASE + 4),
            },
        ];
        assert!(plan_flash(&overlap, FLASH_BASE + 1024).is_err());
        let missing_address = [FlashPart {
            file: overlap[1].file.clone(),
            addr: None,
        }];
        assert!(plan_flash(&missing_address, FLASH_BASE + 1024).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn accepts_exact_flash_end_and_rejects_one_byte_past() {
        let dir = temp_dir();
        let exact = dir.join("exact.bin");
        let past = dir.join("past.bin");
        fs::write(&exact, [0; 4]).unwrap();
        fs::write(&past, [0; 5]).unwrap();
        let make = |file| FlashPart {
            file,
            addr: Some(FLASH_BASE),
        };
        assert!(validate_part_range(&make(exact), FLASH_BASE + 4).is_ok());
        assert!(validate_part_range(&make(past), FLASH_BASE + 4).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
