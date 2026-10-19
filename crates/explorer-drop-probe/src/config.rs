use serde::Deserialize;
use std::{
    ffi::OsString,
    io::{self, Read},
    os::windows::ffi::OsStringExt,
    path::{Component, Path, PathBuf, Prefix},
};

pub const MAX_CONFIG_BYTES: usize = 512 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u32,
    enabled: bool,
    gui: Vec<u16>,
    journal: Option<Vec<u16>>,
}
#[derive(Clone, Debug)]
pub struct Config {
    pub gui: PathBuf,
    pub journal: Option<PathBuf>,
}
pub fn location() -> io::Result<PathBuf> {
    let root = PathBuf::from(
        std::env::var_os("LOCALAPPDATA")
            .ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?,
    );
    if !root.is_absolute() {
        return Err(io::Error::other("LOCALAPPDATA is not absolute"));
    }
    Ok(root.join("VartaCopy/explorer-transfer.json"))
}
fn native(units: Vec<u16>) -> io::Result<PathBuf> {
    if units.is_empty() || units.len() > 32767 || units.contains(&0) {
        return Err(io::Error::other("invalid native configuration path"));
    }
    let path = PathBuf::from(OsString::from_wide(&units));
    if !local(&path) {
        return Err(io::Error::other(
            "shell configuration requires absolute local paths",
        ));
    }
    Ok(path)
}
fn local(path: &Path) -> bool {
    path.is_absolute()
        && matches!(path.components().next(),Some(Component::Prefix(p)) if matches!(p.kind(),Prefix::Disk(_)|Prefix::VerbatimDisk(_)))
        && !path.components().any(|p| p == Component::ParentDir)
}
pub fn decode(reader: impl Read) -> io::Result<Option<Config>> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::other("shell configuration is oversized"));
    }
    let wire: Wire = serde_json::from_slice(&bytes)?;
    if wire.version != 1 {
        return Err(io::Error::other("unsupported shell configuration version"));
    }
    if !wire.enabled {
        return Ok(None);
    }
    let gui = native(wire.gui)?;
    if !gui
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("vartacopy-gui.exe"))
    {
        return Err(io::Error::other("shell GUI must be vartacopy-gui.exe"));
    }
    Ok(Some(Config {
        gui,
        journal: wire.journal.map(native).transpose()?,
    }))
}
pub fn read() -> io::Result<Option<Config>> {
    let file = match std::fs::File::open(location()?) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let config = decode(file)?;
    if let Some(config) = &config
        && !config.gui.is_file()
    {
        return Err(io::Error::other(
            "configured VartaCopy executable is missing",
        ));
    }
    Ok(config)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_is_bounded_and_accepts_only_the_own_gui_and_local_paths() {
        let wire = |path: &str| {
            serde_json::json!({"version":1,"enabled":true,"gui":path.encode_utf16().collect::<Vec<_>>(),"journal":null}).to_string()
        };
        let good = wire(r"D:\VartaCopy\vartacopy-gui.exe");
        assert!(decode(good.as_bytes()).unwrap().is_some());
        for bad in [
            r"D:\cmd.exe",
            r"\\host\share\vartacopy-gui.exe",
            r"D:\a\..\vartacopy-gui.exe",
            "relative",
        ] {
            assert!(decode(wire(bad).as_bytes()).is_err());
        }
        assert!(decode(io::repeat(b' ')).is_err());
        assert!(decode(b"{}".as_slice()).is_err());
        assert!(
            decode(b"{\"version\":1,\"enabled\":false,\"gui\":[],\"journal\":null}".as_slice())
                .unwrap()
                .is_none()
        );
    }
}
