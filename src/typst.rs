use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use snafu::ResultExt;

use crate::Result;

pub fn run(arguments: &[OsString], packages: &Path) -> Result<i32> {
    let mut command = Command::new("typst");
    command.args(arguments).env("TYPST_PACKAGE_PATH", packages);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
            .whatever_context("could not run Typst; install typst and ensure it is on PATH")
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .whatever_context("could not run Typst; install typst and ensure it is on PATH")?;
        Ok(status.code().unwrap_or(1))
    }
}
