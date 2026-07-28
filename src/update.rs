use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const BINARY_NAME: &str = "codex-agent-indicator";
const LAUNCH_AGENT_LABEL: &str = "com.codex-agent-indicator";

#[derive(Debug, Eq, PartialEq)]
struct UpdatePlan {
    executable: PathBuf,
    install_root: PathBuf,
}

impl UpdatePlan {
    fn from_executable(executable: &Path) -> Result<Self> {
        if executable.file_name() != Some(OsStr::new(BINARY_NAME)) {
            bail!(
                "cannot update executable {}; expected it to be named {BINARY_NAME}",
                executable.display()
            );
        }

        let bin_directory = executable
            .parent()
            .context("cannot determine the executable directory")?;
        if bin_directory.file_name() != Some(OsStr::new("bin")) {
            bail!(
                "cannot determine the install root for {}; expected the executable inside a bin directory",
                executable.display()
            );
        }
        let install_root = bin_directory
            .parent()
            .context("cannot determine the Cargo install root")?;

        Ok(Self {
            executable: executable.to_path_buf(),
            install_root: install_root.to_path_buf(),
        })
    }

    fn cargo_arguments(&self) -> Vec<OsString> {
        [
            OsString::from("install"),
            OsString::from(BINARY_NAME),
            OsString::from("--registry"),
            OsString::from("crates-io"),
            OsString::from("--locked"),
            OsString::from("--force"),
            OsString::from("--root"),
            self.install_root.clone().into_os_string(),
        ]
        .into()
    }
}

pub fn run() -> Result<()> {
    if let Some(argument) = env::args().nth(2) {
        bail!("update does not accept arguments, got {argument:?}");
    }

    let executable = env::current_exe().context("failed to locate the running executable")?;
    let plan = UpdatePlan::from_executable(&executable)?;

    println!(
        "Updating {BINARY_NAME} from crates.io in {}...",
        plan.install_root.display()
    );
    let status = Command::new("cargo")
        .args(plan.cargo_arguments())
        .status()
        .context("failed to run cargo; install Rust and ensure cargo is on PATH")?;
    if !status.success() {
        bail!("cargo install failed with {status}");
    }

    let output = Command::new(&plan.executable)
        .arg("--version")
        .output()
        .with_context(|| {
            format!(
                "updated the package but failed to verify {}",
                plan.executable.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "updated the package but {} --version failed with {}",
            plan.executable.display(),
            output.status
        );
    }
    println!("{}", String::from_utf8_lossy(&output.stdout).trim());

    restart_launch_agent_if_managed(&plan);
    Ok(())
}

fn restart_launch_agent_if_managed(plan: &UpdatePlan) {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let managed_executable = home.join(".local/bin").join(BINARY_NAME);
    let launch_agent = home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"));
    if plan.executable != managed_executable || !launch_agent.exists() {
        return;
    }

    let uid = match Command::new("/usr/bin/id").arg("-u").output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            eprintln!(
                "Warning: updated the binary but could not determine the user ID; restart the indicator service or your Mac to load it."
            );
            return;
        }
    };
    let service = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
    match Command::new("/bin/launchctl")
        .args(["kickstart", "-k", &service])
        .status()
    {
        Ok(status) if status.success() => println!("Restarted the keyboard indicator service."),
        _ => eprintln!(
            "Warning: updated the binary but could not restart the keyboard indicator service; run the complete installer or restart your Mac."
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{BINARY_NAME, UpdatePlan};

    #[test]
    fn targets_the_root_containing_the_running_binary() {
        let plan = UpdatePlan::from_executable(Path::new(
            "/Users/example/.local/bin/codex-agent-indicator",
        ))
        .expect("valid install layout");

        assert_eq!(plan.install_root, Path::new("/Users/example/.local"));
        assert_eq!(
            plan.cargo_arguments(),
            [
                "install",
                BINARY_NAME,
                "--registry",
                "crates-io",
                "--locked",
                "--force",
                "--root",
                "/Users/example/.local",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn supports_the_default_cargo_install_layout() {
        let plan = UpdatePlan::from_executable(Path::new(
            "/Users/example/.cargo/bin/codex-agent-indicator",
        ))
        .expect("valid Cargo install layout");

        assert_eq!(plan.install_root, Path::new("/Users/example/.cargo"));
    }

    #[test]
    fn rejects_a_binary_outside_an_install_root() {
        let error = UpdatePlan::from_executable(Path::new(
            "/Users/example/Projects/indicator/target/release/codex-agent-indicator",
        ))
        .expect_err("source build is not an installed binary");

        assert!(
            error
                .to_string()
                .contains("expected the executable inside a bin directory")
        );
    }

    #[test]
    fn rejects_a_renamed_binary() {
        let error =
            UpdatePlan::from_executable(Path::new("/Users/example/.local/bin/indicator"))
                .expect_err("renamed binaries cannot be updated in place");

        assert!(error.to_string().contains(BINARY_NAME));
    }
}
