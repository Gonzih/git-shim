use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::{self, Command, ExitStatus, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};

const INTERNAL_CLEANER_ENV: &str = "GIT_SHIM_INTERNAL_CLEANER";
const INTERNAL_CLEANER_COMMAND: &str = "--git-shim-clean-message";
const TARGET_TRAILER: &[u8] = b"Claude Opus 5.5 (1M context) noreply@anthropic.com";
const TARGET_TRAILER_WITH_ANGLE_BRACKETS: &[u8] =
    b"Claude Opus 5.5 (1M context) <noreply@anthropic.com>";

static TEMP_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn main() {
    let exit_code = match run() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("git-shim: {error}");
            1
        }
    };

    process::exit(exit_code);
}

fn run() -> Result<i32, Box<dyn Error>> {
    let arguments: Vec<OsString> = env::args_os().skip(1).collect();

    if is_internal_cleaner(&arguments) {
        if arguments.len() != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "internal cleaner needs exactly one message path",
            )
            .into());
        }

        clean_message(Path::new(&arguments[1]))?;
        return Ok(0);
    }

    let executable = env::current_exe()?;
    let executable = fs::canonicalize(&executable).unwrap_or(executable);
    let real_git = find_real_git(&executable)?;

    let status = if let Some(command_index) = find_command_index(&arguments) {
        if arguments[command_index] == OsStr::new("commit") {
            if let Some(overlay) =
                HookOverlay::create(&real_git, &arguments[..command_index], &executable)?
            {
                let status = run_with_overlay(&real_git, &arguments, command_index, &overlay)?;
                drop(overlay);
                status
            } else {
                run_real_git(&real_git, &arguments)?
            }
        } else {
            run_real_git(&real_git, &arguments)?
        }
    } else {
        run_real_git(&real_git, &arguments)?
    };

    Ok(exit_code(status))
}

fn is_internal_cleaner(arguments: &[OsString]) -> bool {
    env::var_os(INTERNAL_CLEANER_ENV).as_deref() == Some(OsStr::new("1"))
        && arguments.first().map(OsString::as_os_str) == Some(OsStr::new(INTERNAL_CLEANER_COMMAND))
}

fn find_real_git(this_executable: &Path) -> io::Result<PathBuf> {
    let path = env::var_os("PATH").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "PATH is not set, so the real git executable cannot be found",
        )
    })?;

    for directory in env::split_paths(&path) {
        let candidate = directory.join("git");
        if !is_executable(&candidate) {
            continue;
        }

        let candidate = match fs::canonicalize(candidate) {
            Ok(candidate) => candidate,
            Err(_) => continue,
        };

        if candidate != this_executable {
            return Ok(candidate);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not find a real git executable on PATH",
    ))
}

fn run_real_git(real_git: &Path, arguments: &[OsString]) -> io::Result<ExitStatus> {
    Command::new(real_git).args(arguments).status()
}

fn run_with_overlay(
    real_git: &Path,
    arguments: &[OsString],
    command_index: usize,
    overlay: &HookOverlay,
) -> io::Result<ExitStatus> {
    let mut hooks_path = OsString::from("core.hooksPath=");
    hooks_path.push(overlay.path());

    Command::new(real_git)
        .args(&arguments[..command_index])
        .arg("-c")
        .arg(hooks_path)
        .args(&arguments[command_index..])
        .status()
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn find_command_index(arguments: &[OsString]) -> Option<usize> {
    let mut index = 0;

    while index < arguments.len() {
        let argument = arguments[index].to_str()?;

        if argument == "--" {
            return None;
        }

        if !argument.starts_with('-') || argument == "-" {
            return Some(index);
        }

        if matches!(
            argument,
            "-C" | "-c"
                | "--config-env"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
        ) {
            index += 2;
            if index > arguments.len() {
                return None;
            }
            continue;
        }

        index += 1;
    }

    None
}

struct HookOverlay {
    directory: TemporaryDirectory,
}

impl HookOverlay {
    fn create(
        real_git: &Path,
        global_arguments: &[OsString],
        cleaner: &Path,
    ) -> Result<Option<Self>, Box<dyn Error>> {
        let git_directory_output = probe_git(
            real_git,
            global_arguments,
            &["rev-parse", "--absolute-git-dir"],
        )?;
        if !git_directory_output.status.success() {
            return Ok(None);
        }
        let git_directory = path_from_output(&git_directory_output)?;

        let bare_output = probe_git(
            real_git,
            global_arguments,
            &["rev-parse", "--is-bare-repository"],
        )?;
        if !bare_output.status.success() {
            return Ok(None);
        }
        let is_bare = text_from_output(&bare_output)? == "true";

        let hook_working_directory = if is_bare {
            git_directory.clone()
        } else {
            let top_level_output = probe_git(
                real_git,
                global_arguments,
                &["rev-parse", "--show-toplevel"],
            )?;
            if !top_level_output.status.success() {
                return Ok(None);
            }
            path_from_output(&top_level_output)?
        };

        let hooks_path_output = probe_git(
            real_git,
            global_arguments,
            &["config", "--path", "--get", "core.hooksPath"],
        )?;
        let original_hooks = if hooks_path_output.status.success() {
            let configured_path = path_from_output(&hooks_path_output)?;
            if configured_path.is_absolute() {
                configured_path
            } else {
                hook_working_directory.join(configured_path)
            }
        } else if hooks_path_output.status.code() == Some(1) {
            let default_hooks_output = probe_git(
                real_git,
                global_arguments,
                &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
            )?;
            if !default_hooks_output.status.success() {
                return Ok(None);
            }
            path_from_output(&default_hooks_output)?
        } else {
            return Ok(None);
        };

        let overlay = Self {
            directory: TemporaryDirectory::create("git-shim-hooks")?,
        };
        overlay.populate(&original_hooks, cleaner)?;
        Ok(Some(overlay))
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn populate(&self, original_hooks: &Path, cleaner: &Path) -> io::Result<()> {
        let mut original_commit_msg = None;

        if let Ok(entries) = fs::read_dir(original_hooks) {
            for entry in entries.flatten() {
                let original_hook = entry.path();
                if !is_executable(&original_hook) {
                    continue;
                }

                let name = entry.file_name();
                if name == OsStr::new("commit-msg") {
                    original_commit_msg = Some(original_hook);
                } else {
                    write_hook_wrapper(&self.path().join(name), &original_hook)?;
                }
            }
        }

        write_commit_msg_wrapper(
            &self.path().join("commit-msg"),
            original_commit_msg.as_deref(),
            cleaner,
        )
    }
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn create(prefix: &str) -> io::Result<Self> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for _ in 0..100 {
            let sequence = TEMP_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("{prefix}-{}-{timestamp}-{sequence}", process::id()));

            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique temporary hooks directory",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn probe_git(
    real_git: &Path,
    global_arguments: &[OsString],
    arguments: &[&str],
) -> io::Result<Output> {
    Command::new(real_git)
        .args(global_arguments)
        .args(arguments)
        .stderr(process::Stdio::null())
        .output()
}

fn text_from_output(output: &Output) -> io::Result<String> {
    let mut text = String::from_utf8(output.stdout.clone()).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Git returned a non-UTF-8 path: {error}"),
        )
    })?;
    while text.ends_with(['\n', '\r']) {
        text.pop();
    }
    Ok(text)
}

fn path_from_output(output: &Output) -> io::Result<PathBuf> {
    Ok(PathBuf::from(text_from_output(output)?))
}

fn write_hook_wrapper(destination: &Path, original_hook: &Path) -> io::Result<()> {
    let mut script = b"#!/bin/sh\nexec ".to_vec();
    script.extend(shell_quote(original_hook));
    script.extend_from_slice(b" \"$@\"\n");
    write_executable(destination, &script)
}

fn write_commit_msg_wrapper(
    destination: &Path,
    original_hook: Option<&Path>,
    cleaner: &Path,
) -> io::Result<()> {
    let mut script = b"#!/bin/sh\n".to_vec();

    if let Some(original_hook) = original_hook {
        script.extend(shell_quote(original_hook));
        script.extend_from_slice(
            b" \"$@\"\nstatus=$?\nif [ \"$status\" -ne 0 ]; then\n  exit \"$status\"\nfi\n",
        );
    }

    script.extend_from_slice(INTERNAL_CLEANER_ENV.as_bytes());
    script.extend_from_slice(b"=1 exec ");
    script.extend(shell_quote(cleaner));
    script.extend_from_slice(b" ");
    script.extend_from_slice(INTERNAL_CLEANER_COMMAND.as_bytes());
    script.extend_from_slice(b" \"$1\"\n");
    write_executable(destination, &script)
}

fn write_executable(path: &Path, contents: &[u8]) -> io::Result<()> {
    fs::write(path, contents)?;

    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions)?;
    }

    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> Vec<u8> {
    let mut quoted = Vec::with_capacity(path.as_os_str().as_bytes().len() + 2);
    quoted.push(b'\'');

    for byte in path.as_os_str().as_bytes() {
        if *byte == b'\'' {
            quoted.extend_from_slice(b"'\"'\"'");
        } else {
            quoted.push(*byte);
        }
    }

    quoted.push(b'\'');
    quoted
}

#[cfg(not(unix))]
fn shell_quote(path: &Path) -> Vec<u8> {
    format!("\"{}\"", path.display()).into_bytes()
}

fn clean_message(path: &Path) -> io::Result<()> {
    let message = fs::read(path)?;
    let filtered = strip_target_trailer(&message);

    if filtered != message {
        fs::write(path, filtered)?;
    }

    Ok(())
}

fn strip_target_trailer(message: &[u8]) -> Vec<u8> {
    let mut filtered = Vec::with_capacity(message.len());
    let mut start = 0;

    while start < message.len() {
        let end = message[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| start + offset + 1)
            .unwrap_or(message.len());
        let line = &message[start..end];

        if !is_target_trailer(line) {
            filtered.extend_from_slice(line);
        }

        start = end;
    }

    filtered
}

fn is_target_trailer(line: &[u8]) -> bool {
    let line = without_line_ending(line);
    let line = trim_ascii_whitespace(line);
    let Some(separator) = line.iter().position(|byte| *byte == b':') else {
        return false;
    };

    let key = trim_ascii_whitespace(&line[..separator]);
    let value = trim_ascii_whitespace(&line[separator + 1..]);

    key.eq_ignore_ascii_case(b"co-authored-by")
        && (value == TARGET_TRAILER || value == TARGET_TRAILER_WITH_ANGLE_BRACKETS)
}

fn without_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn trim_ascii_whitespace(value: &[u8]) -> &[u8] {
    let first = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let last = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(first);

    &value[first..last]
}
