use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
};

use super::TestCommand;

#[derive(Clone)]
pub(super) struct TestEnvironment {
    variables: Vec<(OsString, OsString)>,
}

impl TestEnvironment {
    pub(super) fn new(root: &Path, system_git: &Path) -> Self {
        let home = root.join("home");
        let temporary_directory = root.join("tmp");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&temporary_directory).unwrap();

        let git_config = home.join("gitconfig");
        fs::write(
            &git_config,
            "[color]\n\tui = false\n[commit]\n\tgpgSign = false\n[tag]\n\tgpgSign = false\n",
        )
        .unwrap();

        let mut paths = vec![root.to_path_buf()];
        if let Some(parent) = system_git.parent() {
            push_unique_path(&mut paths, parent);
        }

        #[cfg(unix)]
        for path in ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
            push_unique_path(&mut paths, Path::new(path));
        }

        let mut variables = vec![
            (OsString::from("HOME"), home.clone().into_os_string()),
            (OsString::from("XDG_CONFIG_HOME"), home.join(".config").into_os_string()),
            (OsString::from("TMPDIR"), temporary_directory.clone().into_os_string()),
            (OsString::from("TMP"), temporary_directory.clone().into_os_string()),
            (OsString::from("TEMP"), temporary_directory.into_os_string()),
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_CONFIG_GLOBAL"), git_config.into_os_string()),
            (OsString::from("GIT_ATTR_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
            (OsString::from("GCM_INTERACTIVE"), OsString::from("never")),
            (OsString::from("GIT_PAGER"), OsString::from("cat")),
            (OsString::from("PAGER"), OsString::from("cat")),
            (OsString::from("GIT_EDITOR"), OsString::from("true")),
            (OsString::from("GIT_SEQUENCE_EDITOR"), OsString::from("true")),
            (OsString::from("LANG"), OsString::from("C")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("TZ"), OsString::from("UTC")),
            (OsString::from("TERM"), OsString::from("dumb")),
            (OsString::from("NO_COLOR"), OsString::from("1")),
            (OsString::from("RUST_LOG"), OsString::from("info")),
            (OsString::from("RUST_BACKTRACE"), OsString::from("0")),
            (OsString::from("NO_PROXY"), OsString::from("127.0.0.1,localhost")),
            (OsString::from("no_proxy"), OsString::from("127.0.0.1,localhost")),
        ];

        // Preserve only process-instrumentation settings needed to run the
        // already-built test binaries. These do not provide developer or CI
        // configuration to GHerrit, but dropping them would make coverage and
        // sanitizer runs silently incomplete or unable to start.
        for name in [
            "LLVM_PROFILE_FILE",
            "ASAN_OPTIONS",
            "LSAN_OPTIONS",
            "MSAN_OPTIONS",
            "TSAN_OPTIONS",
            "UBSAN_OPTIONS",
        ] {
            if let Some(value) = env::var_os(name) {
                variables.push((OsString::from(name), value));
            }
        }

        #[cfg(target_os = "linux")]
        if let Some(value) = env::var_os("LD_LIBRARY_PATH") {
            variables.push((OsString::from("LD_LIBRARY_PATH"), value));
        }

        #[cfg(target_os = "macos")]
        for name in ["DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
            if let Some(value) = env::var_os(name) {
                variables.push((OsString::from(name), value));
            }
        }

        #[cfg(windows)]
        {
            if let Some(system_root) = env::var_os("SystemRoot") {
                let system_root = PathBuf::from(system_root);
                push_unique_path(&mut paths, &system_root.join("System32"));
                variables
                    .push((OsString::from("SystemRoot"), system_root.clone().into_os_string()));
                variables.push((OsString::from("WINDIR"), system_root.clone().into_os_string()));
                let command_processor = env::var_os("COMSPEC")
                    .unwrap_or_else(|| system_root.join("System32/cmd.exe").into_os_string());
                variables.push((OsString::from("COMSPEC"), command_processor));
            }

            if let Some(git_root) = system_git.parent().and_then(Path::parent) {
                push_unique_path(&mut paths, &git_root.join("mingw64/bin"));
                push_unique_path(&mut paths, &git_root.join("usr/bin"));
            }
            variables.push((OsString::from("PATHEXT"), OsString::from(".COM;.EXE;.BAT;.CMD")));
        }

        variables.push((OsString::from("PATH"), env::join_paths(paths).unwrap()));
        Self { variables }
    }

    pub(super) fn apply_to_command(&self, cmd: &mut TestCommand) {
        cmd.env_clear();
        cmd.envs(self.variables.iter().cloned());
    }

    pub(super) fn command(&self, program: &Path) -> TestCommand {
        let mut cmd = TestCommand::new(program);
        self.apply_to_command(&mut cmd);
        cmd
    }
}

pub(super) fn push_unique_path(paths: &mut Vec<PathBuf>, path: &Path) {
    if !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_path_buf());
    }
}

pub(super) static SYSTEM_GIT: LazyLock<PathBuf> = LazyLock::new(|| -> PathBuf {
    let output = if cfg!(windows) {
        Command::new("where").arg("git").output()
    } else {
        Command::new("which").arg("git").output()
    };
    let output = output.expect("Failed to find system git");
    if !output.status.success() {
        panic!("Failed to find git using 'which/where': {:?}", output);
    }
    let stdout = String::from_utf8(output.stdout).expect("Invalid utf8 from which git");
    let path = stdout.lines().next().expect("No git path found").trim();
    PathBuf::from(path)
});
