//! LSP document paths and URI conversion.

use std::path::Path;
use std::path::PathBuf;

#[must_use]
pub fn language_id_for_path(path: &Path) -> Option<&'static str> {
    let filename = path.file_name().and_then(|name| name.to_str());
    if filename.is_some_and(|name| name == "Dockerfile" || name.starts_with("Dockerfile.")) {
        return Some("dockerfile");
    }
    match filename {
        Some("go.mod") => return Some("gomod"),
        Some("go.work") => return Some("gowork"),
        _ => {}
    }
    let extension = path.extension().and_then(|extension| extension.to_str());
    if matches!(extension, Some("C" | "H")) {
        return Some("cpp");
    }
    let extension = extension.map(str::to_ascii_lowercase);
    let builtin = match extension.as_deref().unwrap_or_default() {
        "rs" => Some("rust"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("typescriptreact"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "jsx" => Some("javascriptreact"),
        "py" | "pyi" | "pyw" => Some("python"),
        "go" => Some("go"),
        "gotmpl" => Some("gotmpl"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("cpp"),
        "json" => Some("json"),
        "jsonc" => Some("jsonc"),
        "toml" => Some("toml"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "scss" => Some("scss"),
        "sass" => Some("sass"),
        "less" => Some("less"),
        "yaml" | "yml" => Some("yaml"),
        "md" | "markdown" => Some("markdown"),
        "mdx" => Some("markdown.mdx"),
        "sh" | "bash" | "zsh" => Some("shellscript"),
        "fish" => Some("fish"),
        "lua" => Some("lua"),
        "php" => Some("php"),
        "svelte" => Some("svelte"),
        "vue" => Some("vue"),
        "astro" => Some("astro"),
        "tf" => Some("terraform"),
        "tfvars" => Some("terraform-vars"),
        "asm" => Some("asm"),
        _ => None,
    };
    builtin.or_else(|| crate::tools::mason_filetype_for_path(path))
}

#[must_use]
pub fn file_uri(path: &Path) -> String {
    let mut encoded = String::new();
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    };
    #[cfg(windows)]
    let path_text = {
        let Some(text) = path.to_str() else {
            use std::fmt::Write;
            use std::os::windows::ffi::OsStrExt;
            // Internal keys must not alias a valid filename. This is never sent to LSP.
            let mut key = String::from("dusty-path:");
            for unit in path.as_os_str().encode_wide() {
                let _ = write!(key, "{unit:04X}");
            }
            return key;
        };
        let text = text.replace('\\', "/");
        let text = if let Some(unc) = text.strip_prefix("//?/UNC/") {
            format!("//{unc}")
        } else {
            text.strip_prefix("//?/").unwrap_or(&text).to_owned()
        };
        if text.as_bytes().get(1) == Some(&b':') {
            format!("/{text}")
        } else {
            text
        }
    };
    #[cfg(windows)]
    let bytes = path_text.as_bytes();
    for &byte in bytes {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~')
            || (cfg!(windows) && byte == b':')
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    #[cfg(windows)]
    if encoded.starts_with("//") {
        return format!("file:{encoded}");
    }
    format!("file://{encoded}")
}

/// Only send paths that the file URI decoder addresses without changing the native path.
pub(super) fn wire_file_uri(path: &Path) -> Option<String> {
    let uri = file_uri(path);
    let decoded = path_from_file_uri(&uri)?;
    same_file_path(path, &decoded).then_some(uri)
}

pub(crate) fn same_file_path(left: &Path, right: &Path) -> bool {
    if left.as_os_str() == right.as_os_str() {
        return true;
    }
    #[cfg(unix)]
    {
        false
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut left = left.components();
        let mut right = right.components();
        let same_prefix = match (left.next(), right.next()) {
            (Some(Component::Prefix(left)), Some(Component::Prefix(right))) => {
                match (left.kind(), right.kind()) {
                    (
                        Prefix::Disk(a) | Prefix::VerbatimDisk(a),
                        Prefix::Disk(b) | Prefix::VerbatimDisk(b),
                    ) => a.eq_ignore_ascii_case(&b),
                    (
                        Prefix::UNC(a, s) | Prefix::VerbatimUNC(a, s),
                        Prefix::UNC(b, t) | Prefix::VerbatimUNC(b, t),
                    ) => a.eq_ignore_ascii_case(b) && s.eq_ignore_ascii_case(t),
                    _ => false,
                }
            }
            _ => false,
        };
        // Do not fold case in the remaining path: Windows directories can opt
        // into case-sensitive names. Do not erase verbatim-only path components.
        same_prefix
            && left.clone().all(|part| match part {
                Component::RootDir => true,
                Component::Normal(name) => windows_ordinary_component(name),
                _ => false,
            })
            && left.eq(right)
    }
}

#[cfg(windows)]
fn windows_ordinary_component(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.ends_with(['.', ' ']) || name.contains(['/', '\\', ':']) {
        return false;
    }
    // DOS device names are literal filenames only in the verbatim namespace.
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

#[cfg(all(test, windows))]
#[test]
fn windows_wire_file_uris_preserve_drive_unc_localhost_and_native_names() {
    for (path, alias, expected) in [
        (r"c:\Work\x.rs", r"\\?\C:\Work\x.rs", "file:///c:/Work/x.rs"),
        (
            r"\\SERVER\Share\project\x.rs",
            r"\\?\UNC\server\SHARE\project\x.rs",
            "file://SERVER/Share/project/x.rs",
        ),
        (
            r"\\localhost\Share\project",
            r"\\?\UNC\LOCALHOST\share\project",
            "file://localhost/Share/project",
        ),
        (
            r"C:\Work\a b-é.rs",
            r"\\?\C:\Work\a b-é.rs",
            "file:///C:/Work/a%20b-%C3%A9.rs",
        ),
        (
            r"\\localhost\Share\a b-é.rs",
            r"\\?\UNC\localhost\Share\a b-é.rs",
            "file://localhost/Share/a%20b-%C3%A9.rs",
        ),
    ] {
        let p = Path::new(path);
        let a = Path::new(alias);
        let uri = wire_file_uri(p).expect("normal Windows file path");
        assert!(uri.eq_ignore_ascii_case(expected), "{uri}");
        let decoded = path_from_file_uri(&uri).unwrap();
        assert!(same_file_path(&decoded, p));
        assert!(same_file_path(&decoded, a));
        assert!(wire_file_uri(a).is_some());
    }
    assert!(!same_file_path(
        Path::new(r"C:\src\A.rs"),
        Path::new(r"C:\src\a.rs")
    ));
}

#[cfg(all(test, windows))]
#[test]
fn language_server_refuses_a_native_path_that_would_alias_replacement_character() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let mut path: Vec<u16> = r"C:\project\".encode_utf16().collect();
    path.extend([0xD800, u16::from(b'.'), u16::from(b'r'), u16::from(b's')]);
    let surrogate = PathBuf::from(OsString::from_wide(&path));
    let replacement = Path::new(r"C:\project\�.rs");
    assert_ne!(file_uri(&surrogate), file_uri(replacement));
    let wire = wire_file_uri(replacement).expect("valid UTF-8 path");
    assert_eq!(path_from_file_uri(&wire).as_deref(), Some(replacement));
    assert!(wire_file_uri(&surrogate).is_none());
}

#[must_use]
pub fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let source = uri.strip_prefix("file://")?;
    let (authority, source) = source.split_at(source.find('/')?);
    if authority
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, ':' | '@' | '\\' | '%'))
    {
        return None;
    }
    #[cfg(unix)]
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let bytes = source.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let value = u8::from_str_radix(
                std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?,
                16,
            )
            .ok()?;
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
    }
    #[cfg(windows)]
    {
        let text = String::from_utf8(decoded).ok()?;
        // Open buffers use canonical paths, which keep the verbatim prefix only past the
        // ordinary Windows length limit (see `files::canonicalize`).
        let (plain, verbatim) = if !authority.is_empty()
            && !(authority.eq_ignore_ascii_case("localhost")
                && text.as_bytes().get(2) == Some(&b':'))
        {
            let share = format!(r"{authority}{}", text.replace('/', r"\"));
            (format!(r"\\{share}"), format!(r"\\?\UNC\{share}"))
        } else if text.as_bytes().get(2) == Some(&b':') {
            let disk = text.strip_prefix('/')?.replace('/', r"\");
            (disk.clone(), format!(r"\\?\{disk}"))
        } else {
            return Some(PathBuf::from(text));
        };
        Some(PathBuf::from(if plain.len() < 260 {
            plain
        } else {
            verbatim
        }))
    }
}
