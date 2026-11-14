//! Open local output directories without passing user paths through a shell.
use crate::{
    error::{invalid, Result},
    i18n::Language,
};
use std::{
    path::Path,
    process::{Command, Stdio},
};

fn command(path: &Path, language: Language) -> Result<Command> {
    if !path.is_dir() {
        return Err(invalid(language.text(
            "The output folder does not exist yet. Check the output path.",
            "保存フォルダーがまだありません。保存先を確認してください。",
        )));
    }
    let path = path.canonicalize()?;
    #[cfg(target_os = "windows")]
    let mut command = {
        // Explorer does not consistently accept canonicalize's extended-length prefix.
        use std::{
            ffi::OsString,
            os::windows::ffi::{OsStrExt, OsStringExt},
        };
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
        let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
        let arg = if units.starts_with(&unc) {
            OsString::from_wide(
                &[
                    r"\\".encode_utf16().collect::<Vec<_>>(),
                    units[unc.len()..].to_vec(),
                ]
                .concat(),
            )
        } else if units.starts_with(&prefix) {
            OsString::from_wide(&units[prefix.len()..])
        } else {
            path.into_os_string()
        };
        let mut cmd = Command::new("explorer.exe");
        cmd.arg(arg);
        cmd
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut cmd = Command::new("open");
        cmd.arg(path);
        cmd
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut command = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(path);
        cmd
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

pub fn open(path: &Path, language: Language) -> Result<()> {
    let mut child = command(path, language)?.spawn().map_err(|_| {
        invalid(language.text(
            "Cannot open the folder. Check your file manager settings.",
            "フォルダーを開けませんでした。ファイルマネージャーの設定を確認してください。",
        ))
    })?;
    // Explorer can return nonzero after forwarding to an existing window. Do not
    // block the TUI waiting on the window; reap helpers without taking over stdin.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn japanese_path_with_spaces_and_shell_characters_is_one_argument() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("保存先 空白 & $(literal)");
        std::fs::create_dir(&path).unwrap();
        let cmd = command(&path, Language::English).unwrap();
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args.len(), 1);
        assert!(args[0]
            .to_string_lossy()
            .ends_with("保存先 空白 & $(literal)"));
        #[cfg(target_os = "windows")]
        {
            assert_eq!(cmd.get_program(), "explorer.exe");
            assert!(!args[0].to_string_lossy().starts_with(r"\\?\"));
        }
    }
    #[test]
    fn never_creates_a_missing_output_or_opens_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing");
        assert!(command(&path, Language::English).is_err());
        assert!(!path.exists());
        std::fs::write(&path, b"not a directory").unwrap();
        assert!(command(&path, Language::English).is_err());
    }
}
