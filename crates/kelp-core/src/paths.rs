//! Lossless project-relative paths. UTF-8 names retain their existing encoding.
//! Non-UTF-8 components use NUL + hex; NUL cannot be a filesystem name byte.
#[cfg(not(unix))]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};

fn component(part: &str) -> Result<Vec<u8>> {
    if let Some(hex) = part.strip_prefix('\0') {
        ensure!(
            !hex.is_empty()
                && hex.len().is_multiple_of(2)
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid raw path encoding"
        );
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()?;
        ensure!(
            std::str::from_utf8(&bytes).is_err(),
            "noncanonical raw path encoding"
        );
        Ok(bytes)
    } else {
        Ok(part.as_bytes().to_vec())
    }
}

pub fn validate(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "empty file path");
    for part in path.split('/') {
        let bytes = component(part)?;
        ensure!(
            !bytes.is_empty()
                && bytes != b"."
                && bytes != b".."
                && !bytes.contains(&0)
                && !bytes.contains(&b'/')
                && !bytes.eq_ignore_ascii_case(b".kelp")
                && !bytes.eq_ignore_ascii_case(b".git"),
            "unsupported file path: {}",
            display(path)
        );
    }
    Ok(())
}

pub fn from_bytes(bytes: &[u8]) -> Result<String> {
    let path = bytes
        .split(|byte| *byte == b'/')
        .map(|part| {
            std::str::from_utf8(part)
                .map(str::to_owned)
                .unwrap_or_else(|_| {
                    let mut encoded = String::from("\0");
                    for byte in part {
                        encoded.push_str(&format!("{byte:02x}"));
                    }
                    encoded
                })
        })
        .collect::<Vec<_>>()
        .join("/");
    validate(&path)?;
    Ok(path)
}

pub fn from_native(path: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        from_bytes(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        from_bytes(
            path.to_str()
                .context("path is not representable as UTF-8 on this platform")?
                .replace(std::path::MAIN_SEPARATOR, "/")
                .as_bytes(),
        )
    }
}

pub fn to_native(path: &str) -> Result<PathBuf> {
    validate(path)?;
    let mut result = PathBuf::new();
    for part in path.split('/') {
        let bytes = component(part)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            result.push(std::ffi::OsString::from_vec(bytes));
        }
        #[cfg(not(unix))]
        {
            let text = std::str::from_utf8(&bytes)
                .context("this filesystem cannot represent a non-UTF-8 path")?;
            let stem = text
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            ensure!(
                !text.ends_with(['.', ' '])
                    && !text
                        .chars()
                        .any(|c| c.is_control() || "\\:<>\"|?*".contains(c))
                    && ![
                        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
                        "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
                        "LPT7", "LPT8", "LPT9"
                    ]
                    .contains(&stem.as_str()),
                "this filesystem cannot represent {}",
                display(path)
            );
            result.push(text);
        }
    }
    Ok(result)
}

pub fn display(path: &str) -> String {
    path.split('/')
        .map(|part| {
            if part.starts_with('\0') {
                component(part)
                    .map(|bytes| bytes.iter().map(|byte| format!("\\x{byte:02x}")).collect())
                    .unwrap_or_else(|_| part.escape_debug().to_string())
            } else {
                part.chars()
                    .flat_map(|c| {
                        if c.is_control() {
                            c.escape_default().collect::<Vec<_>>()
                        } else {
                            vec![c]
                        }
                    })
                    .collect()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}
