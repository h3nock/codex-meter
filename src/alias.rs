use std::{
    env, fs,
    path::{Path, PathBuf},
};

use crate::error::{AppError, AppResult};

const SHORT_NAME: &str = "cm";

pub fn print_status() -> AppResult<()> {
    match find_command_in_current_path(SHORT_NAME) {
        Some(path) => {
            println!("cm is already used by {}", path.display());
            Ok(())
        }
        None => {
            println!("cm is available on this PATH");
            Ok(())
        }
    }
}

pub fn create(bin_dir: Option<PathBuf>) -> AppResult<()> {
    let target = env::current_exe()
        .map_err(|source| AppError::io("failed to locate current executable", source))?;
    let bin_dir = match bin_dir {
        Some(path) => path,
        None => target
            .parent()
            .ok_or_else(|| AppError::Alias("current executable has no parent directory".into()))?
            .to_path_buf(),
    };

    fs::create_dir_all(&bin_dir).map_err(|source| {
        AppError::io(format!("failed to create {}", bin_dir.display()), source)
    })?;

    let destination = bin_dir.join(SHORT_NAME);
    validate_alias_destination(&target, &destination)?;

    create_symlink(&target, &destination)?;
    println!("created cm -> {}", target.display());
    Ok(())
}

fn validate_alias_destination(target: &Path, destination: &Path) -> AppResult<()> {
    if let Some(existing) = find_command_in_current_path(SHORT_NAME) {
        if same_file(&existing, target) {
            println!("cm already points to {}", target.display());
            return Ok(());
        }

        return Err(AppError::Alias(format!(
            "cm already resolves to {}; refusing to overwrite another command",
            existing.display()
        )));
    }

    if destination.exists() && !same_file(destination, target) {
        return Err(AppError::Alias(format!(
            "{} already exists; refusing to overwrite it",
            destination.display()
        )));
    }

    Ok(())
}

fn find_command_in_current_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    find_command_in_path(command, &path)
}

fn find_command_in_path(command: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata.permissions().mode() & 0o111 != 0,
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path) -> AppResult<()> {
    if destination.exists() && same_file(destination, target) {
        return Ok(());
    }

    std::os::unix::fs::symlink(target, destination).map_err(|source| {
        AppError::io(
            format!(
                "failed to create alias {} -> {}",
                destination.display(),
                target.display()
            ),
            source,
        )
    })
}

#[cfg(not(unix))]
fn create_symlink(_: &Path, _: &Path) -> AppResult<()> {
    Err(AppError::Alias(
        "alias create currently supports Unix-like systems only".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn detects_command_collision_in_supplied_path() {
        let root = unique_temp_dir("codex-meter-alias");
        fs::create_dir_all(&root).expect("create temp dir");
        let cm = root.join("cm");
        fs::write(&cm, "#!/bin/sh\n").expect("write cm");

        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&cm).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&cm, permissions).expect("chmod");
        }

        let found = find_command_in_path("cm", &OsString::from(root.as_os_str()));
        assert_eq!(found.as_deref(), Some(cm.as_path()));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn ignores_non_executable_path_entries() {
        let root = unique_temp_dir("codex-meter-alias-nonexec");
        fs::create_dir_all(&root).expect("create temp dir");
        fs::write(root.join("cm"), "not executable").expect("write cm");

        #[cfg(unix)]
        assert!(find_command_in_path("cm", &OsString::from(root.as_os_str())).is_none());

        fs::remove_dir_all(root).expect("cleanup");
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        env::temp_dir().join(format!("{prefix}-{nanos}"))
    }
}
