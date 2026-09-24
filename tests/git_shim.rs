use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::{self, Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const TARGET: &str = "Co-Authored-By: Claude Opus 5.5 (1M context) noreply@anthropic.com";
const TARGET_WITH_ANGLE_BRACKETS: &str =
    "co-authored-by: Claude Opus 5.5 (1M context) <noreply@anthropic.com>";
const OTHER_COAUTHOR: &str = "Co-Authored-By: Ada Lovelace <ada@example.test>";

static TEMP_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn strips_target_trailer_from_m_messages_and_keeps_other_content() {
    let repository = TestRepository::new();
    repository.stage_change("message.txt", "one\n");

    let message = format!("Subject\n\nBody stays.\n\n{TARGET}\n{OTHER_COAUTHOR}");
    repository
        .commit_with_shim(&["commit", "-m", &message])
        .success();

    let committed = repository.commit_message();
    assert!(!committed.contains(TARGET));
    assert!(committed.contains("Body stays."));
    assert!(committed.contains(OTHER_COAUTHOR));
}

#[test]
fn strips_target_trailer_from_file_messages() {
    let repository = TestRepository::new();
    repository.stage_change("message.txt", "one\n");
    let message_file = repository.path().join("message.txt");
    fs::write(
        &message_file,
        format!("File subject\n\n{TARGET_WITH_ANGLE_BRACKETS}\n{OTHER_COAUTHOR}\n"),
    )
    .unwrap();

    repository
        .commit_with_shim_os(&[
            OsString::from("commit"),
            OsString::from("-F"),
            message_file.into_os_string(),
        ])
        .success();

    let committed = repository.commit_message();
    assert!(!committed.contains("Claude Opus 5.5 (1M context)"));
    assert!(committed.contains(OTHER_COAUTHOR));
}

#[test]
fn strips_target_trailer_after_the_editor_runs() {
    let repository = TestRepository::new();
    repository.stage_change("editor.txt", "one\n");
    let editor = repository.path().join("write-message");
    write_script(
        &editor,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' 'Editor subject' '' 'Editor body' '' '{TARGET}' '{OTHER_COAUTHOR}' > \"$1\"\n"
        ),
    );

    repository
        .commit_with_shim_os_and_env(
            &[OsString::from("commit")],
            [(OsStr::new("GIT_EDITOR"), editor.as_os_str())],
        )
        .success();

    let committed = repository.commit_message();
    assert!(!committed.contains(TARGET));
    assert!(committed.contains("Editor body"));
    assert!(committed.contains(OTHER_COAUTHOR));
}

#[test]
fn preserves_configured_executable_hooks_and_filters_after_commit_msg() {
    let repository = TestRepository::new();
    let hooks = repository.path().join("custom-hooks");
    fs::create_dir(&hooks).unwrap();
    repository
        .real_git(&["config", "core.hooksPath", "custom-hooks"])
        .success();
    write_script(
        &hooks.join("pre-commit"),
        "#!/bin/sh\nprintf ran > .pre-commit-ran\n",
    );
    write_script(
        &hooks.join("commit-msg"),
        &format!("#!/bin/sh\nprintf '%s\\n' '{OTHER_COAUTHOR}' >> \"$1\"\n"),
    );

    repository.stage_change("hook.txt", "one\n");
    repository
        .commit_with_shim(&["commit", "-m", &format!("Hook subject\n\n{TARGET}")])
        .success();

    assert_eq!(
        fs::read_to_string(repository.path().join(".pre-commit-ran")).unwrap(),
        "ran"
    );
    let committed = repository.commit_message();
    assert!(!committed.contains(TARGET));
    assert!(committed.contains(OTHER_COAUTHOR));
}

#[test]
fn preserves_default_hooks_in_a_linked_worktree() {
    let repository = TestRepository::new();
    repository.stage_change("base.txt", "base\n");
    repository.real_git(&["commit", "-m", "base"]).success();

    let worktree_parent = TemporaryDirectory::create("git-shim-linked-worktree").unwrap();
    let worktree = worktree_parent.path().join("linked");
    Command::new(&repository.real_git)
        .current_dir(repository.path())
        .args(["worktree", "add", "--quiet"])
        .arg(&worktree)
        .output()
        .unwrap()
        .success();

    let hooks_output = repository
        .real_git(&["rev-parse", "--git-path", "hooks"])
        .success();
    let hooks = path_from_git_output(repository.path(), hooks_output);
    write_script(
        &hooks.join("pre-commit"),
        "#!/bin/sh\nprintf ran > .linked-worktree-hook-ran\n",
    );

    fs::write(worktree.join("linked.txt"), "one\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_git"))
        .current_dir(&worktree)
        .args(["add", "linked.txt"])
        .output()
        .unwrap()
        .success();
    let message = format!("Linked worktree\n\n{TARGET}");
    Command::new(env!("CARGO_BIN_EXE_git"))
        .current_dir(&worktree)
        .args(["commit", "-m", &message])
        .output()
        .unwrap()
        .success();

    assert_eq!(
        fs::read_to_string(worktree.join(".linked-worktree-hook-ran")).unwrap(),
        "ran"
    );
    let committed = Command::new(&repository.real_git)
        .current_dir(&worktree)
        .args(["log", "-1", "--format=%B"])
        .output()
        .unwrap()
        .success();
    assert!(
        !String::from_utf8(committed.stdout)
            .unwrap()
            .contains(TARGET)
    );
}

#[test]
fn failing_existing_hook_still_prevents_the_commit() {
    let repository = TestRepository::new();
    let hooks = repository.path().join(".git/hooks");
    write_script(
        &hooks.join("commit-msg"),
        "#!/bin/sh\nprintf ran > .failing-hook-ran\nexit 42\n",
    );

    repository.stage_change("failure.txt", "one\n");
    let output = repository.commit_with_shim(&["commit", "-m", &format!("Failure\n\n{TARGET}")]);

    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(repository.path().join(".failing-hook-ran")).unwrap(),
        "ran"
    );
    assert!(
        !repository
            .real_git(&["rev-parse", "--verify", "HEAD"])
            .status
            .success()
    );
}

#[test]
fn no_verify_bypasses_the_commit_msg_filter() {
    let repository = TestRepository::new();
    repository.stage_change("no-verify.txt", "one\n");
    repository
        .commit_with_shim(&[
            "commit",
            "--no-verify",
            "-m",
            &format!("No verify\n\n{TARGET}"),
        ])
        .success();

    assert!(repository.commit_message().contains(TARGET));
}

#[test]
fn non_commit_commands_and_failed_exit_statuses_pass_through() {
    let repository = TestRepository::new();
    fs::write(repository.path().join("untracked.txt"), "one\n").unwrap();

    let status = repository
        .commit_with_shim(&["status", "--porcelain"])
        .success();
    assert!(
        String::from_utf8(status.stdout)
            .unwrap()
            .contains("?? untracked.txt")
    );

    let direct = repository.real_git(&["rev-parse", "--verify", "missing-reference"]);
    let shim = repository.commit_with_shim(&["rev-parse", "--verify", "missing-reference"]);
    assert_eq!(shim.status.code(), direct.status.code());
}

struct TestRepository {
    directory: TemporaryDirectory,
    real_git: PathBuf,
}

impl TestRepository {
    fn new() -> Self {
        let directory = TemporaryDirectory::create("git-shim-test").unwrap();
        let real_git = find_real_git();
        let repository = Self {
            directory,
            real_git,
        };

        repository.real_git(&["init", "--quiet"]).success();
        repository
            .real_git(&["config", "user.name", "Git Shim Test"])
            .success();
        repository
            .real_git(&["config", "user.email", "git-shim@example.test"])
            .success();
        repository
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn stage_change(&self, path: &str, contents: &str) {
        fs::write(self.path().join(path), contents).unwrap();
        self.real_git(&["add", path]).success();
    }

    fn real_git(&self, arguments: &[&str]) -> Output {
        Command::new(&self.real_git)
            .current_dir(self.path())
            .args(arguments)
            .output()
            .unwrap()
    }

    fn commit_with_shim(&self, arguments: &[&str]) -> Output {
        self.commit_with_shim_os(&arguments.iter().map(OsString::from).collect::<Vec<_>>())
    }

    fn commit_with_shim_os(&self, arguments: &[OsString]) -> Output {
        self.commit_with_shim_os_and_env(arguments, std::iter::empty())
    }

    fn commit_with_shim_os_and_env<'a, I>(&self, arguments: &[OsString], environment: I) -> Output
    where
        I: IntoIterator<Item = (&'a OsStr, &'a OsStr)>,
    {
        let mut command = Command::new(env!("CARGO_BIN_EXE_git"));
        command.current_dir(self.path()).args(arguments);
        for (key, value) in environment {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn commit_message(&self) -> String {
        let output = self.real_git(&["log", "-1", "--format=%B"]).success();
        String::from_utf8(output.stdout).unwrap()
    }
}

fn find_real_git() -> PathBuf {
    let shim = fs::canonicalize(env!("CARGO_BIN_EXE_git")).unwrap();
    let path = env::var_os("PATH").expect("PATH must be set for integration tests");

    env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .filter_map(|candidate| fs::canonicalize(candidate).ok())
        .find(|candidate| candidate != &shim && is_executable(candidate))
        .expect("a real git executable must be available on PATH")
}

fn path_from_git_output(base: &Path, output: Output) -> PathBuf {
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn write_script(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();

    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }
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
            "could not create a unique temporary test directory",
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

trait OutputAssertions {
    fn success(self) -> Self;
}

impl OutputAssertions for Output {
    fn success(self) -> Self {
        assert!(
            self.status.success(),
            "command failed with {:?}\nstdout:\n{}\nstderr:\n{}",
            self.status.code(),
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr),
        );
        self
    }
}
