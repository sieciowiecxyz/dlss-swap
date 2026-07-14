use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = "310.7.0";
const EXPECTED_SHA256: &str = "be6e434a94ca32499515eb62ca0e6c274526055d568d0426e4c652dcdfb6ee6e";
const CNN_E_VERSION: &str = "3.7.0";
const CNN_E_EXPECTED_SHA256: &str =
    "8bea8eee99861d0cdc9b8b90e1fb915f5d96750c2d575ed8d2417452105581df";
const DLL_NAME: &str = "nvngx_dlss.dll";
const BACKUP_SUFFIX: &str = ".dlls-swap-original";

#[derive(Debug, Clone, Copy)]
struct DlssBuild {
    version: &'static str,
    expected_sha256: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Launch(Vec<OsString>),
    Status,
    Restore,
    Help,
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preset {
    E,
    K,
    L,
    M,
}

impl Preset {
    fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "e" | "cnn" | "competitive" | "fast" => Ok(Self::E),
            "k" | "quality" | "default" | "transformer" => Ok(Self::K),
            "m" | "performance" | "low-res" => Ok(Self::M),
            "l" | "ultra-performance" | "ultra-low-res" => Ok(Self::L),
            _ => Err(format!(
                "invalid preset '{value}'; run 'dlss-swap --preset help'"
            )),
        }
    }

    fn env_value(self) -> &'static str {
        match self {
            Self::E => "render_preset_e",
            Self::K => "render_preset_k",
            Self::L => "render_preset_l",
            Self::M => "render_preset_m",
        }
    }

    fn build(self) -> DlssBuild {
        match self {
            Self::E => DlssBuild {
                version: CNN_E_VERSION,
                expected_sha256: CNN_E_EXPECTED_SHA256,
            },
            Self::K | Self::L | Self::M => DlssBuild {
                version: VERSION,
                expected_sha256: EXPECTED_SHA256,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cli {
    dry_run: bool,
    preset: Preset,
    action: Action,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    game_root: PathBuf,
    dll_path: PathBuf,
    fingerprint: Option<Fingerprint>,
    sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
}

#[derive(Debug)]
struct LocatedDll {
    path: PathBuf,
    cache_hit: bool,
}

fn usage() -> &'static str {
    "Usage:\n  dlss-swap [OPTIONS] -- COMMAND [ARGS...]\n  dlss-swap [OPTIONS] status|restore\n\nRun 'dlss-swap --help' for details."
}

fn help() -> &'static str {
    r#"dlss-swap - Replace and configure NVIDIA DLSS Super Resolution

USAGE:
    dlss-swap [OPTIONS] -- COMMAND [ARGS...]
    dlss-swap [OPTIONS] status
    dlss-swap [OPTIONS] restore

OPTIONS:
    -p, --preset <PRESET>
            Select the DLSS Super Resolution model preset.

            Available presets:

              e, cnn, competitive, fast
                  Legacy CNN preset E using the pinned DLSS 3.7.0 DLL.
                  Typically has a lower processing cost than Transformer models.
                  Recommended for competitive games and very high frame rates.

              k, quality, default, transformer
                  First-generation Transformer preset using DLSS 310.7.0.
                  Best general-purpose option for DLAA, Quality and Balanced modes.
                  Recommended when image quality and stability are the priority.

              m, performance, low-res
                  Second-generation Transformer preset optimized for lower
                  internal resolutions and fast motion.
                  Recommended for DLSS Performance, especially at 4K.

              l, ultra-performance, ultra-low-res
                  Transformer preset designed for extremely low internal
                  resolutions.
                  Recommended mainly for 4K Ultra Performance.

            Examples:
              dlss-swap --preset cnn -- %command%
              dlss-swap --preset quality -- %command%
              dlss-swap --preset performance -- %command%
              dlss-swap --preset ultra-performance -- %command%

    --dry-run
            Show what would be changed without modifying any files or launching
            the command.

    --status
            Show the currently installed DLL version and selected target preset.
            The positional command `status` is also accepted.

    --restore
            Restore the original DLSS DLL.
            The positional command `restore` is also accepted.

    -h, --help
            Print help information.

    -V, --version
            Print version information.

PRESET GUIDE:
    Competitive / maximum FPS       e / cnn
    DLAA or DLSS Quality            k / quality
    DLSS Performance                m / performance
    DLSS Ultra Performance          l / ultra-performance

NOTES:
    The preset selects the reconstruction model. It does not change the in-game
    DLSS scaling mode such as Quality, Balanced or Performance.

    Preset aliases are case-insensitive. Preset E uses DLSS 3.7.0; presets K,
    M and L use DLSS 310.7.0. The required DLL is selected automatically."#
}

fn parse_args<I>(args: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    let mut dry_run = false;
    let mut preset = Preset::L;
    let mut position = 0;

    while position < args.len() {
        if args[position] == "--dry-run" {
            dry_run = true;
            position += 1;
        } else if args[position] == "--preset" || args[position] == "-p" {
            let value = args
                .get(position + 1)
                .and_then(|value| value.to_str())
                .ok_or("--preset requires a value; run 'dlss-swap --preset help'")?;
            if value.eq_ignore_ascii_case("help") {
                if position + 2 != args.len() {
                    return Err("--preset help does not accept additional arguments".to_owned());
                }
                return Ok(Cli {
                    dry_run,
                    preset,
                    action: Action::Help,
                });
            }
            preset = Preset::parse(value)?;
            position += 2;
        } else {
            break;
        }
    }

    if position == args.len() {
        return Err(usage().to_owned());
    }

    let action = match args[position].to_str() {
        Some("status" | "--status") if position + 1 == args.len() => Action::Status,
        Some("restore" | "--restore") if position + 1 == args.len() => Action::Restore,
        Some("--help" | "-h") if position + 1 == args.len() => Action::Help,
        Some("--version" | "-V") if position + 1 == args.len() => Action::Version,
        Some("--") => {
            let command = args[(position + 1)..].to_vec();
            if command.is_empty() {
                return Err("missing game command after --".to_owned());
            }
            Action::Launch(command)
        }
        _ => return Err(usage().to_owned()),
    };

    Ok(Cli {
        dry_run,
        preset,
        action,
    })
}

fn run(cli: Cli) -> Result<(), String> {
    if cli.action == Action::Help {
        println!("{}", help());
        return Ok(());
    }
    if cli.action == Action::Version {
        println!("dlss-swap {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let game_root = game_root()?;
    let cache_file = cache_file(&game_root)?;
    let located = locate_dll(&game_root, &cache_file, !cli.dry_run)?;

    match cli.action {
        Action::Status => {
            let build = cli.preset.build();
            let source = source_dll(build)?;
            status(
                &game_root,
                &source,
                &cache_file,
                located,
                build,
                cli.dry_run,
            )
        }
        Action::Restore => restore(&game_root, &cache_file, located, cli.dry_run),
        Action::Launch(command) => {
            let build = cli.preset.build();
            let source = source_dll(build)?;
            swap(
                &game_root,
                &source,
                &cache_file,
                &located,
                cli.preset,
                build,
                cli.dry_run,
            )?;
            if cli.dry_run {
                println!(
                    "dry-run: would exec {} with preset {}",
                    display_command(&command),
                    cli.preset.env_value()
                );
                Ok(())
            } else {
                exec_game(command, cli.preset)
            }
        }
        Action::Help | Action::Version => unreachable!(),
    }
}

pub(crate) fn execute<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = OsString>,
{
    parse_args(args).and_then(run)
}

fn game_root() -> Result<PathBuf, String> {
    let root =
        env::var_os("STEAM_COMPAT_INSTALL_PATH").ok_or("STEAM_COMPAT_INSTALL_PATH is not set")?;
    let root = PathBuf::from(root);
    if !root.is_dir() {
        return Err(format!("game directory does not exist: {}", root.display()));
    }
    fs::canonicalize(&root).map_err(|error| format!("cannot resolve game directory: {error}"))
}

fn source_dll(build: DlssBuild) -> Result<PathBuf, String> {
    let project_source = source_in_root(Path::new(env!("CARGO_MANIFEST_DIR")), build);
    if project_source.is_file() {
        return Ok(project_source);
    }

    if let Ok(executable) = env::current_exe()
        && let Some(root) = executable.parent()
    {
        let installed_source = source_in_root(root, build);
        if installed_source.is_file() {
            return Ok(installed_source);
        }
    }

    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or("HOME and XDG_DATA_HOME are not set")?;
    Ok(data_home
        .join("dlls-swap")
        .join(build.version)
        .join(DLL_NAME))
}

fn source_in_root(root: &Path, build: DlssBuild) -> PathBuf {
    root.join("assets")
        .join("dlss")
        .join(build.version)
        .join(DLL_NAME)
}

fn verify_source(source: &Path, build: DlssBuild) -> Result<(), String> {
    if !source.is_file() {
        return Err(format!("pinned DLSS DLL is missing: {}", source.display()));
    }
    let actual = sha256(source)?;
    if actual != build.expected_sha256 {
        return Err(format!(
            "pinned DLSS {} DLL hash mismatch: expected {}, got {actual}",
            build.version, build.expected_sha256
        ));
    }
    Ok(())
}

fn cache_file(game_root: &Path) -> Result<PathBuf, String> {
    let cache_home = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or("HOME and XDG_CACHE_HOME are not set")?;
    let key = hex_digest(game_root.as_os_str().as_encoded_bytes());
    Ok(cache_home.join("dlls-swap").join(format!("{key}.cache")))
}

fn locate_dll(
    game_root: &Path,
    cache_file: &Path,
    write_cache: bool,
) -> Result<LocatedDll, String> {
    if let Ok(entry) = read_cache(cache_file)
        && entry.game_root == game_root
        && valid_cached_path(game_root, &entry.dll_path)
    {
        return Ok(LocatedDll {
            path: entry.dll_path,
            cache_hit: true,
        });
    }

    let mut matches = Vec::new();
    find_dlls(game_root, &mut matches)?;
    matches.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    let path = matches
        .into_iter()
        .next()
        .ok_or_else(|| format!("{DLL_NAME} was not found below {}", game_root.display()))?;

    if write_cache {
        let entry = CacheEntry {
            game_root: game_root.to_owned(),
            dll_path: path.clone(),
            fingerprint: None,
            sha256: None,
        };
        write_cache_or_warn(cache_file, &entry);
    }

    Ok(LocatedDll {
        path,
        cache_hit: false,
    })
}

fn valid_cached_path(game_root: &Path, path: &Path) -> bool {
    path.starts_with(game_root)
        && path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(DLL_NAME))
}

fn find_dlls(directory: &Path, matches: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot scan {}: {error}", directory.display()))?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("dlss-swap: warning: skipped directory entry: {error}");
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                eprintln!(
                    "dlss-swap: warning: skipped {}: {error}",
                    entry.path().display()
                );
                continue;
            }
        };
        if file_type.is_dir() {
            if let Err(error) = find_dlls(&entry.path(), matches) {
                eprintln!("dlss-swap: warning: {error}");
            }
        } else if file_type.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(DLL_NAME))
        {
            matches.push(entry.path());
        }
    }
    Ok(())
}

fn status(
    game_root: &Path,
    source: &Path,
    cache_file: &Path,
    located: LocatedDll,
    build: DlssBuild,
    dry_run: bool,
) -> Result<(), String> {
    let target_hash = cached_or_actual_hash(game_root, cache_file, &located.path, !dry_run)?.0;
    let backup = backup_path(&located.path);
    println!("game: {}", game_root.display());
    println!("dll: {}", located.path.display());
    println!(
        "path cache: {}",
        if located.cache_hit {
            "hit"
        } else {
            "miss (full scan)"
        }
    );
    println!("dll sha256: {target_hash}");
    println!(
        "selected: DLSS {} ({})",
        build.version,
        if build.version == CNN_E_VERSION {
            "CNN preset E"
        } else {
            "current K/M/L presets"
        }
    );
    println!(
        "pinned: {} ({}; {})",
        source.display(),
        build.expected_sha256,
        if source.is_file() {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "state: {}",
        if target_hash == build.expected_sha256 {
            "selected version installed"
        } else if target_hash == EXPECTED_SHA256 {
            "DLSS 310.7.0 installed"
        } else if target_hash == CNN_E_EXPECTED_SHA256 {
            "DLSS 3.7.0 installed"
        } else {
            "original/different"
        }
    );
    println!(
        "backup: {} ({})",
        backup.display(),
        if backup.is_file() {
            "present"
        } else {
            "missing"
        }
    );
    Ok(())
}

fn swap(
    game_root: &Path,
    source: &Path,
    cache_file: &Path,
    located: &LocatedDll,
    preset: Preset,
    build: DlssBuild,
    dry_run: bool,
) -> Result<(), String> {
    let (target_hash, hash_cache_hit) =
        cached_or_actual_hash(game_root, cache_file, &located.path, !dry_run)?;
    eprintln!(
        "dlss-swap: DLL {} (path cache: {}, hash cache: {})",
        located.path.display(),
        hit_miss(located.cache_hit),
        hit_miss(hash_cache_hit)
    );
    if target_hash == build.expected_sha256 {
        eprintln!(
            "dlss-swap: DLSS {} already installed; preset {}",
            build.version,
            preset.env_value()
        );
        return Ok(());
    }

    verify_source(source, build)?;

    let backup = backup_path(&located.path);
    if backup.exists() && !backup.is_file() {
        return Err(format!("backup path is not a file: {}", backup.display()));
    }
    if !backup.is_file() {
        if dry_run {
            eprintln!(
                "dlss-swap: dry-run: would create backup {}",
                backup.display()
            );
        } else {
            copy_new(&located.path, &backup)?;
            eprintln!("dlss-swap: created one-time backup {}", backup.display());
        }
    }

    if dry_run {
        eprintln!(
            "dlss-swap: dry-run: would atomically install DLSS {}; preset {}",
            build.version,
            preset.env_value()
        );
        return Ok(());
    }

    atomic_replace(source, &located.path)?;
    update_cached_hash(game_root, cache_file, &located.path, build.expected_sha256);
    eprintln!(
        "dlss-swap: atomically installed DLSS {}; preset {} enabled",
        build.version,
        preset.env_value()
    );
    Ok(())
}

fn restore(
    game_root: &Path,
    cache_file: &Path,
    located: LocatedDll,
    dry_run: bool,
) -> Result<(), String> {
    let backup = backup_path(&located.path);
    if !backup.is_file() {
        return Err(format!("backup does not exist: {}", backup.display()));
    }
    let backup_hash = sha256(&backup)?;
    if dry_run {
        println!(
            "dry-run: would atomically restore {} ({backup_hash}) to {}",
            backup.display(),
            located.path.display()
        );
        return Ok(());
    }
    atomic_replace(&backup, &located.path)?;
    update_cached_hash(game_root, cache_file, &located.path, &backup_hash);
    println!("restored {} ({backup_hash})", located.path.display());
    Ok(())
}

fn cached_or_actual_hash(
    game_root: &Path,
    cache_file: &Path,
    dll_path: &Path,
    write_cache: bool,
) -> Result<(String, bool), String> {
    let metadata = fs::metadata(dll_path)
        .map_err(|error| format!("cannot stat {}: {error}", dll_path.display()))?;
    let fingerprint = Fingerprint::from_metadata(&metadata);
    if let Ok(entry) = read_cache(cache_file)
        && entry.game_root == game_root
        && entry.dll_path == dll_path
        && entry.fingerprint == Some(fingerprint)
        && let Some(hash) = entry.sha256
    {
        return Ok((hash, true));
    }
    let hash = sha256(dll_path)?;
    if write_cache {
        let entry = CacheEntry {
            game_root: game_root.to_owned(),
            dll_path: dll_path.to_owned(),
            fingerprint: Some(fingerprint),
            sha256: Some(hash.clone()),
        };
        write_cache_or_warn(cache_file, &entry);
    }
    Ok((hash, false))
}

fn update_cached_hash(game_root: &Path, cache_file: &Path, dll_path: &Path, hash: &str) {
    let metadata = match fs::metadata(dll_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!(
                "dlss-swap: warning: cache update failed: cannot stat {}: {error}",
                dll_path.display()
            );
            return;
        }
    };
    write_cache_or_warn(
        cache_file,
        &CacheEntry {
            game_root: game_root.to_owned(),
            dll_path: dll_path.to_owned(),
            fingerprint: Some(Fingerprint::from_metadata(&metadata)),
            sha256: Some(hash.to_owned()),
        },
    );
}

impl Fingerprint {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.len(),
            mtime_sec: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        }
    }
}

fn read_cache(path: &Path) -> Result<CacheEntry, String> {
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value = |key: &str| {
        contents.lines().find_map(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
        })
    };
    if value("format") != Some("1") {
        return Err("unsupported cache format".to_owned());
    }
    let game_root = PathBuf::from(value("game_root").ok_or("missing game_root")?);
    let dll_path = PathBuf::from(value("dll_path").ok_or("missing dll_path")?);
    let sha256 = value("sha256")
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let nonempty = |key: &str| value(key).filter(|value| !value.is_empty());
    let fingerprint = match (
        nonempty("dev"),
        nonempty("ino"),
        nonempty("size"),
        nonempty("mtime_sec"),
        nonempty("mtime_nsec"),
    ) {
        (Some(dev), Some(ino), Some(size), Some(sec), Some(nsec)) => Some(Fingerprint {
            dev: dev.parse().map_err(|_| "invalid dev")?,
            ino: ino.parse().map_err(|_| "invalid ino")?,
            size: size.parse().map_err(|_| "invalid size")?,
            mtime_sec: sec.parse().map_err(|_| "invalid mtime_sec")?,
            mtime_nsec: nsec.parse().map_err(|_| "invalid mtime_nsec")?,
        }),
        _ => None,
    };
    Ok(CacheEntry {
        game_root,
        dll_path,
        fingerprint,
        sha256,
    })
}

fn write_cache_file(path: &Path, entry: &CacheEntry) -> Result<(), String> {
    let parent = path.parent().ok_or("cache path has no parent")?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let temp = sibling_temp(path);
    let fingerprint = entry.fingerprint;
    let contents = format!(
        "format=1\ngame_root={}\ndll_path={}\ndev={}\nino={}\nsize={}\nmtime_sec={}\nmtime_nsec={}\nsha256={}\n",
        entry.game_root.display(),
        entry.dll_path.display(),
        fingerprint
            .map(|value| value.dev.to_string())
            .unwrap_or_default(),
        fingerprint
            .map(|value| value.ino.to_string())
            .unwrap_or_default(),
        fingerprint
            .map(|value| value.size.to_string())
            .unwrap_or_default(),
        fingerprint
            .map(|value| value.mtime_sec.to_string())
            .unwrap_or_default(),
        fingerprint
            .map(|value| value.mtime_nsec.to_string())
            .unwrap_or_default(),
        entry.sha256.as_deref().unwrap_or_default(),
    );
    fs::write(&temp, contents)
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    fs::rename(&temp, path)
        .map_err(|error| format!("cannot install cache {}: {error}", path.display()))
}

fn write_cache_or_warn(path: &Path, entry: &CacheEntry) {
    if let Err(error) = write_cache_file(path, entry) {
        eprintln!("dlss-swap: warning: cache update failed: {error}");
    }
}

fn backup_path(dll: &Path) -> PathBuf {
    let mut name = dll.as_os_str().to_owned();
    name.push(BACKUP_SUFFIX);
    PathBuf::from(name)
}

fn copy_new(source: &Path, destination: &Path) -> Result<(), String> {
    let mut input =
        File::open(source).map_err(|error| format!("cannot open {}: {error}", source.display()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
    io::copy(&mut input, &mut output).map_err(|error| format!("cannot copy backup: {error}"))?;
    output
        .sync_all()
        .map_err(|error| format!("cannot sync backup: {error}"))
}

fn atomic_replace(source: &Path, destination: &Path) -> Result<(), String> {
    let temp = sibling_temp(destination);
    fs::copy(source, &temp).map_err(|error| {
        format!(
            "cannot copy {} to {}: {error}",
            source.display(),
            temp.display()
        )
    })?;
    let file =
        File::open(&temp).map_err(|error| format!("cannot open {}: {error}", temp.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", temp.display()))?;
    if let Err(error) = fs::rename(&temp, destination) {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "cannot atomically replace {}: {error}",
            destination.display()
        ));
    }
    Ok(())
}

fn sibling_temp(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".dlls-swap.tmp.{}", std::process::id()));
    PathBuf::from(name)
}

fn sha256(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn exec_game(command: Vec<OsString>, preset: Preset) -> Result<(), String> {
    use std::os::unix::process::CommandExt;

    let mut process = Command::new(&command[0]);
    process.args(&command[1..]);
    process.env("DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE", "on");
    process.env(
        "DXVK_NVAPI_DRS_NGX_DLSS_SR_OVERRIDE_RENDER_PRESET_SELECTION",
        preset.env_value(),
    );
    process.env_remove("PROTON_ENABLE_NGX_UPDATER");
    process.env_remove("PROTON_DLSS_UPGRADE");
    let error = process.exec();
    Err(format!(
        "cannot exec {}: {error}",
        command[0].to_string_lossy()
    ))
}

fn display_command(command: &[OsString]) -> String {
    command
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn hit_miss(hit: bool) -> &'static str {
    if hit { "hit" } else { "miss" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            env::temp_dir().join(format!("dlls-swap-{label}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parses_launch_and_dry_run() {
        let cli = parse_args(
            ["--dry-run", "--preset", "m", "--", "game", "argument"].map(OsString::from),
        )
        .unwrap();
        assert!(cli.dry_run);
        assert_eq!(cli.preset, Preset::M);
        assert_eq!(
            cli.action,
            Action::Launch(vec![OsString::from("game"), OsString::from("argument")])
        );
    }

    #[test]
    fn preset_e_selects_dlss_370_cnn_build() {
        let cli = parse_args(["--preset", "e", "--", "game"].map(OsString::from)).unwrap();
        assert_eq!(cli.preset, Preset::E);
        assert_eq!(cli.preset.env_value(), "render_preset_e");
        assert_eq!(cli.preset.build().version, CNN_E_VERSION);
        assert_eq!(cli.preset.build().expected_sha256, CNN_E_EXPECTED_SHA256);
    }

    #[test]
    fn builds_project_local_source_path() {
        assert_eq!(
            source_in_root(Path::new("/project"), Preset::E.build()),
            Path::new("/project/assets/dlss/3.7.0/nvngx_dlss.dll")
        );
        assert_eq!(
            source_in_root(Path::new("/project"), Preset::L.build()),
            Path::new("/project/assets/dlss/310.7.0/nvngx_dlss.dll")
        );
    }

    #[test]
    fn parses_human_friendly_preset_aliases_case_insensitively() {
        for (alias, expected) in [
            ("CNN", Preset::E),
            ("competitive", Preset::E),
            ("quality", Preset::K),
            ("TRANSFORMER", Preset::K),
            ("performance", Preset::M),
            ("low-res", Preset::M),
            ("ultra-performance", Preset::L),
            ("ULTRA-LOW-RES", Preset::L),
        ] {
            let cli = parse_args(["-p", alias, "--", "game"].map(OsString::from)).unwrap();
            assert_eq!(cli.preset, expected, "alias: {alias}");
        }
    }

    #[test]
    fn parses_help_status_restore_and_version_forms() {
        assert_eq!(
            parse_args(["--preset", "help"].map(OsString::from))
                .unwrap()
                .action,
            Action::Help
        );
        assert_eq!(
            parse_args(["--status"].map(OsString::from)).unwrap().action,
            Action::Status
        );
        assert_eq!(
            parse_args(["--restore"].map(OsString::from))
                .unwrap()
                .action,
            Action::Restore
        );
        assert_eq!(
            parse_args(["-V"].map(OsString::from)).unwrap().action,
            Action::Version
        );
    }

    #[test]
    fn finds_then_uses_cached_path() {
        let root = temp_dir("locate");
        let nested = root.join("bin/deep");
        fs::create_dir_all(&nested).unwrap();
        let dll = nested.join(DLL_NAME);
        fs::write(&dll, b"test").unwrap();
        let cache = root.join("cache/state");

        let first = locate_dll(&root, &cache, true).unwrap();
        assert_eq!(first.path, dll);
        assert!(!first.cache_hit);
        let second = locate_dll(&root, &cache, true).unwrap();
        assert_eq!(second.path, dll);
        assert!(second.cache_hit);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_cached_path_triggers_rescan() {
        let root = temp_dir("invalid-cache");
        let dll = root.join(DLL_NAME);
        fs::write(&dll, b"test").unwrap();
        let cache = root.join("cache/state");
        let stale = CacheEntry {
            game_root: root.clone(),
            dll_path: root.join("missing/nvngx_dlss.dll"),
            fingerprint: None,
            sha256: None,
        };
        write_cache_file(&cache, &stale).unwrap();

        let located = locate_dll(&root, &cache, true).unwrap();
        assert_eq!(located.path, dll);
        assert!(!located.cache_hit);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unchanged_file_uses_cached_hash() {
        let root = temp_dir("hash-cache");
        let dll = root.join(DLL_NAME);
        fs::write(&dll, b"content").unwrap();
        let cache = root.join("cache/state");

        let (first, first_hit) = cached_or_actual_hash(&root, &cache, &dll, true).unwrap();
        let (second, second_hit) = cached_or_actual_hash(&root, &cache, &dll, true).unwrap();
        assert_eq!(first, second);
        assert!(!first_hit);
        assert!(second_hit);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_name_matches_contract() {
        assert_eq!(
            backup_path(Path::new("/game/nvngx_dlss.dll")),
            Path::new("/game/nvngx_dlss.dll.dlls-swap-original")
        );
    }

    #[test]
    fn cache_write_failure_does_not_block_discovery_or_hashing() {
        let root = temp_dir("cache-write-failure");
        let dll = root.join(DLL_NAME);
        fs::write(&dll, b"content").unwrap();
        let blocked_parent = root.join("not-a-directory");
        fs::write(&blocked_parent, b"file").unwrap();
        let cache = blocked_parent.join("state");

        let located = locate_dll(&root, &cache, true).unwrap();
        let (_, cache_hit) = cached_or_actual_hash(&root, &cache, &dll, true).unwrap();

        assert_eq!(located.path, dll);
        assert!(!cache_hit);
        fs::remove_dir_all(root).unwrap();
    }
}
