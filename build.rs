use std::fs;
use std::io;

#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;

fn main() -> io::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");

    for (link, target) in [
        (
            "src/typst_args.rs",
            "../vendor/typst/crates/typst-cli/src/args.rs",
        ),
        ("LICENSE-APACHE", "vendor/typst/LICENSE"),
    ] {
        println!("cargo::rerun-if-changed={link}");
        match fs::symlink_metadata(link) {
            // Cargo flattens these links into regular files in published crates.
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => symlink(target, link)?,
            Err(error) => return Err(error),
        }
    }

    Ok(())
}
