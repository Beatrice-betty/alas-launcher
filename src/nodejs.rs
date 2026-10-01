#[cfg(windows)]
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read, Write},
    mem::size_of,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Command,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

#[cfg(windows)]
use anyhow::{bail, Context, Result};
#[cfg(windows)]
use rand::RngCore;
#[cfg(windows)]
use reqwest::blocking::Client;
#[cfg(windows)]
use sha2::{Digest, Sha256};
#[cfg(windows)]
use tracing::{info, warn};

#[cfg(windows)]
use crate::{setup::SplashUpdate, window_util::CreateNoWindow as _};
#[cfg(windows)]
use rust_i18n::t;
#[cfg(windows)]
use winapi::{
    shared::sddl::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
    um::{
        fileapi::CreateDirectoryW, minwinbase::SECURITY_ATTRIBUTES,
        sysinfoapi::GetSystemDirectoryW, winbase::LocalFree, winnt::PSECURITY_DESCRIPTOR,
    },
};

// Keep the installer URL and checksum together so the downloaded executable is
// authenticated independently of the network response that carries it.
#[cfg(windows)]
const NODEJS_LTS_VERSION: &str = "24.21.0";
#[cfg(windows)]
const NODEJS_LTS_X64_MSI_URL: &str = "https://nodejs.org/dist/v24.21.0/node-v24.21.0-x64.msi";
#[cfg(windows)]
const NODEJS_LTS_X64_MSI_SHA256: &str =
    "bb0eaee134f9357f22aea915ee793343e627aefc1e66488164bac6915bce2cac";
#[cfg(windows)]
const NODEJS_LTS_ARM64_MSI_URL: &str = "https://nodejs.org/dist/v24.21.0/node-v24.21.0-arm64.msi";
#[cfg(windows)]
const NODEJS_LTS_ARM64_MSI_SHA256: &str =
    "22ca85110f26015696a3fa9216bc372ae65203d170622eaf7d211e2dd5bb49e3";
#[cfg(windows)]
const NODEJS_LTS_X86_VERSION: &str = "22.22.2";
#[cfg(windows)]
const NODEJS_LTS_X86_MSI_URL: &str = "https://nodejs.org/dist/v22.22.2/node-v22.22.2-x86.msi";
#[cfg(windows)]
const NODEJS_LTS_X86_MSI_SHA256: &str =
    "e43cf42f461cbfea23a079925cfdd132a18cf66d4e30f64ec5ab4ec31dbb41f3";
#[cfg(windows)]
const NODEJS_DOWNLOAD_BUFFER_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const NODEJS_INSTALLER_POLL_INTERVAL: Duration = Duration::from_millis(200);
#[cfg(windows)]
const NODEJS_REGISTRY_PATH: &str = r"SOFTWARE\Node.js";
#[cfg(windows)]
const WINDOWS_CURRENT_VERSION_REGISTRY_PATH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion";
#[cfg(windows)]
const NODEJS_INSTALL_PATH_VALUE: &str = "InstallPath";
// Protected DACL: only SYSTEM and elevated Administrators can alter files;
// the owner-rights denial prevents a medium-integrity owner from rewriting it.
#[cfg(windows)]
const NODEJS_SECURE_INSTALLER_DIRECTORY_SDDL: &str =
    "D:P(D;OICI;WDWO;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
#[cfg(windows)]
const NODEJS_SECURE_INSTALLER_DIRECTORY_ATTEMPTS: usize = 32;
#[cfg(windows)]
const NODEJS_SECURE_INSTALLER_DIRECTORY_RANDOM_BYTES: usize = 16;

#[cfg(windows)]
#[derive(Debug)]
struct NodeJsInstallation {
    executable: PathBuf,
    version: String,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug)]
struct NodeJsInstaller {
    version: &'static str,
    url: &'static str,
    sha256: &'static str,
}

#[cfg(windows)]
struct SecureNodeJsInstallerDir {
    path: PathBuf,
}

#[cfg(windows)]
impl SecureNodeJsInstallerDir {
    fn new() -> Result<Self> {
        let root = windows_temp_directory()?;
        for _ in 0..NODEJS_SECURE_INSTALLER_DIRECTORY_ATTEMPTS {
            let path = root.join(format!("AzurPilot-NodeJs-{}", secure_directory_suffix()));
            match create_secure_directory(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "create protected Node.js installer directory {}",
                            path.display()
                        )
                    })
                }
            }
        }

        bail!("Unable to allocate a protected Node.js installer directory")
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(windows)]
impl Drop for SecureNodeJsInstallerDir {
    fn drop(&mut self) {
        let installer = self.path.join("nodejs-lts.msi");
        if let Err(error) = fs::remove_file(&installer) {
            if error.kind() != io::ErrorKind::NotFound {
                warn!(path = %installer.display(), "Unable to remove Node.js installer: {error}");
            }
        }
        if let Err(error) = fs::remove_dir(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                warn!(path = %self.path.display(), "Unable to remove Node.js installer directory: {error}");
            }
        }
    }
}

#[cfg(windows)]
pub fn is_nodejs_available() -> bool {
    let Some(installation) = find_nodejs_installation() else {
        return false;
    };

    if let Some(directory) = installation
        .executable
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
    {
        prepend_to_path(directory);
    }
    info!(
        version = installation.version,
        executable = %installation.executable.display(),
        "Node.js runtime is available"
    );
    true
}

#[cfg(windows)]
pub fn install_nodejs(
    cancel_requested: &AtomicBool,
    mut status_updater: impl FnMut(SplashUpdate),
) -> Result<()> {
    if is_nodejs_available() {
        return Ok(());
    }
    if cancel_requested.load(Ordering::SeqCst) {
        bail!(t!("setup.cancel_cleaning"));
    }

    let installer = nodejs_installer_for_current_architecture()?;
    validate_nodejs_installer_source(installer.url, installer.sha256)?;
    let installer_dir = SecureNodeJsInstallerDir::new()?;
    let installer_path = installer_dir.path().join("nodejs-lts.msi");

    status_updater(SplashUpdate::loading(
        t!("setup.installing_nodejs"),
        t!("setup.downloading_nodejs", version = installer.version),
        5,
    ));
    download_nodejs_installer(installer, &installer_path, cancel_requested)?;
    verify_nodejs_installer(&installer_path, installer.sha256)?;

    status_updater(SplashUpdate::loading(
        t!("setup.installing_nodejs"),
        t!("setup.installing_nodejs"),
        7,
    ));
    run_nodejs_installer(&installer_path, cancel_requested, &mut status_updater)?;

    if is_nodejs_available() {
        return Ok(());
    }

    bail!(
        "Node.js installer finished, but node.exe was not found in the current process environment"
    );
}

#[cfg(windows)]
fn find_nodejs_installation() -> Option<NodeJsInstallation> {
    for executable in trusted_nodejs_candidates() {
        if let Some(installation) = probe_node(&executable) {
            return Some(installation);
        }
    }
    None
}

#[cfg(windows)]
fn probe_node(executable: &Path) -> Option<NodeJsInstallation> {
    if !executable.is_absolute() || !executable.is_file() {
        return None;
    }
    let output = Command::new(executable)
        .arg("--version")
        .create_no_window()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    Some(NodeJsInstallation {
        executable: executable.to_path_buf(),
        version: parse_nodejs_version(&output.stdout)?,
    })
}

#[cfg(windows)]
fn trusted_nodejs_candidates() -> Vec<PathBuf> {
    // This launcher is elevated, so never resolve node.exe from PATH or CWD.
    // These machine-level registry entries are protected from standard users.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(key) = windows_registry::LOCAL_MACHINE.open(NODEJS_REGISTRY_PATH) {
        if let Ok(install_path) = key.get_string(NODEJS_INSTALL_PATH_VALUE) {
            push_unique_absolute_path(
                &mut candidates,
                PathBuf::from(install_path).join("node.exe"),
            );
        }
    }

    if let Ok(key) = windows_registry::LOCAL_MACHINE.open(WINDOWS_CURRENT_VERSION_REGISTRY_PATH) {
        for value_name in [
            "ProgramFilesDir",
            "ProgramW6432Dir",
            "ProgramFilesDir (x86)",
        ] {
            if let Ok(root) = key.get_string(value_name) {
                push_unique_absolute_path(
                    &mut candidates,
                    PathBuf::from(root).join("nodejs").join("node.exe"),
                );
            }
        }
    }
    candidates
}

#[cfg(windows)]
fn push_unique_absolute_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if candidate.is_absolute()
        && !paths
            .iter()
            .any(|existing| same_windows_path(existing, &candidate))
    {
        paths.push(candidate);
    }
}

#[cfg(windows)]
fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

#[cfg(windows)]
fn prepend_to_path(directory: &Path) {
    let existing_path = env::var_os("PATH").unwrap_or_default();
    if env::split_paths(&existing_path).any(|existing| same_windows_path(&existing, directory)) {
        return;
    }

    let mut paths = vec![directory.to_path_buf()];
    paths.extend(env::split_paths(&existing_path));
    if let Ok(path) = env::join_paths(paths) {
        env::set_var("PATH", path);
    }
}

#[cfg(windows)]
fn parse_nodejs_version(output: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(output).ok()?.trim();
    let version = value.strip_prefix('v')?;
    let mut components = version.split('.');
    let major = components.next()?.parse::<u32>().ok()?;
    let minor = components.next()?.parse::<u32>().ok()?;
    let patch = components.next()?.parse::<u32>().ok()?;
    if major == 0 && minor == 0 && patch == 0 {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(windows)]
fn nodejs_installer_for_current_architecture() -> Result<NodeJsInstaller> {
    nodejs_installer_for_architecture(env::consts::ARCH)
}

#[cfg(windows)]
fn nodejs_installer_for_architecture(architecture: &str) -> Result<NodeJsInstaller> {
    match architecture {
        "x86_64" => Ok(NodeJsInstaller {
            version: NODEJS_LTS_VERSION,
            url: NODEJS_LTS_X64_MSI_URL,
            sha256: NODEJS_LTS_X64_MSI_SHA256,
        }),
        "aarch64" => Ok(NodeJsInstaller {
            version: NODEJS_LTS_VERSION,
            url: NODEJS_LTS_ARM64_MSI_URL,
            sha256: NODEJS_LTS_ARM64_MSI_SHA256,
        }),
        "x86" => Ok(NodeJsInstaller {
            version: NODEJS_LTS_X86_VERSION,
            url: NODEJS_LTS_X86_MSI_URL,
            sha256: NODEJS_LTS_X86_MSI_SHA256,
        }),
        other => bail!(
            "Node.js automatic installation is not available for Windows architecture {other}"
        ),
    }
}

#[cfg(windows)]
fn validate_nodejs_installer_source(url: &str, digest: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("Node.js installer URL must use HTTPS");
    }
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Node.js installer checksum is not a SHA-256 digest");
    }
    Ok(())
}

#[cfg(windows)]
fn download_nodejs_installer(
    installer: NodeJsInstaller,
    part_path: &Path,
    cancel_requested: &AtomicBool,
) -> Result<()> {
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(15 * 60))
        .build()
        .context("build Node.js download client")?;
    let mut response = client
        .get(installer.url)
        .send()
        .context("download Node.js installer")?
        .error_for_status()
        .context("Node.js installer download returned an error status")?;
    let mut destination = File::create(part_path).context("create Node.js installer part file")?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; NODEJS_DOWNLOAD_BUFFER_BYTES];

    loop {
        if cancel_requested.load(Ordering::SeqCst) {
            bail!(t!("setup.cancel_cleaning"));
        }
        let read = response
            .read(&mut buffer)
            .context("read Node.js installer download")?;
        if read == 0 {
            break;
        }
        destination
            .write_all(&buffer[..read])
            .context("write Node.js installer part file")?;
        digest.update(&buffer[..read]);
    }
    destination
        .flush()
        .context("flush Node.js installer part file")?;

    let actual = format!("{:x}", digest.finalize());
    if !actual.eq_ignore_ascii_case(installer.sha256) {
        bail!(
            "Node.js installer checksum mismatch: expected {}, got {}",
            installer.sha256,
            actual
        );
    }
    Ok(())
}

#[cfg(windows)]
fn verify_nodejs_installer(installer_path: &Path, expected_sha256: &str) -> Result<()> {
    let mut source =
        File::open(installer_path).context("open Node.js installer for verification")?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; NODEJS_DOWNLOAD_BUFFER_BYTES];
    loop {
        let read = source
            .read(&mut buffer)
            .context("read Node.js installer for verification")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }

    let actual = format!("{:x}", digest.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        bail!(
            "Node.js installer checksum mismatch after download: expected {}, got {}",
            expected_sha256,
            actual
        );
    }
    Ok(())
}

#[cfg(windows)]
fn run_nodejs_installer(
    installer_path: &Path,
    cancel_requested: &AtomicBool,
    status_updater: &mut impl FnMut(SplashUpdate),
) -> Result<()> {
    let mut command = Command::new(system_msiexec_path()?);
    command.args(nodejs_msi_args(installer_path));
    let mut child = command
        .create_no_window()
        .spawn()
        .context("start Node.js installer")?;
    let mut wait_ticks = 0u16;

    loop {
        if cancel_requested.load(Ordering::SeqCst) {
            crate::diagnostics::cancel_child(&mut child, "nodejs_installer");
            bail!(t!("setup.cancel_cleaning"));
        }

        if let Some(status) = child.try_wait().context("wait for Node.js installer")? {
            if status.success() || status.code() == Some(3010) {
                return Ok(());
            }
            bail!("Node.js installer exited with {status}");
        }

        wait_ticks = wait_ticks.saturating_add(1);
        if wait_ticks >= 10 {
            wait_ticks = 0;
            status_updater(SplashUpdate::loading(
                t!("setup.installing_nodejs"),
                t!("setup.installing_nodejs"),
                7,
            ));
        }
        thread::sleep(NODEJS_INSTALLER_POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn system_msiexec_path() -> Result<PathBuf> {
    // GetSystemDirectoryW avoids PATH/CWD lookup for an elevated child process.
    let msiexec = system_directory()?.join("msiexec.exe");
    if msiexec.is_file() {
        return Ok(msiexec);
    }
    bail!("Windows Installer was not found at {}", msiexec.display());
}

#[cfg(windows)]
fn windows_temp_directory() -> Result<PathBuf> {
    let system_directory = system_directory()?;
    let windows_directory = system_directory
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Unable to locate the Windows directory"))?;
    let temp_directory = windows_directory.join("Temp");
    if temp_directory.is_dir() {
        return Ok(temp_directory);
    }
    bail!(
        "Windows temporary directory was not found at {}",
        temp_directory.display()
    );
}

#[cfg(windows)]
fn system_directory() -> Result<PathBuf> {
    let mut buffer = vec![0u16; 260];
    loop {
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 {
            bail!("Unable to locate the Windows system directory");
        }
        let length = length as usize;
        if length < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        buffer.resize(length.saturating_add(1), 0);
    }
}

#[cfg(windows)]
fn create_secure_directory(path: &Path) -> io::Result<()> {
    let path = wide_null(path.as_os_str());
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let mut sddl = NODEJS_SECURE_INSTALLER_DIRECTORY_SDDL
        .encode_utf16()
        .collect::<Vec<_>>();
    sddl.push(0);

    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            u32::from(SDDL_REVISION_1),
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let created = unsafe { CreateDirectoryW(path.as_ptr(), &mut attributes) };
    unsafe {
        LocalFree(descriptor);
    }
    if created == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn secure_directory_suffix() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0u8; NODEJS_SECURE_INSTALLER_DIRECTORY_RANDOM_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut suffix = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        suffix.push(HEX[(byte >> 4) as usize] as char);
        suffix.push(HEX[(byte & 0x0f) as usize] as char);
    }
    suffix
}

#[cfg(windows)]
fn nodejs_msi_args(installer_path: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "/i".into(),
        installer_path.as_os_str().to_owned(),
        "/qn".into(),
        "/norestart".into(),
    ]
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn test_nodejs_installer_source_requires_https_and_sha256() {
        let digest = "a".repeat(64);

        assert!(
            validate_nodejs_installer_source("https://nodejs.org/dist/node.msi", &digest).is_ok()
        );
        assert!(
            validate_nodejs_installer_source("http://nodejs.org/dist/node.msi", &digest).is_err()
        );
        assert!(
            validate_nodejs_installer_source("https://nodejs.org/dist/node.msi", "bad").is_err()
        );
    }

    #[test]
    fn test_nodejs_version_parser_rejects_non_node_output() {
        assert_eq!(
            parse_nodejs_version(b"v24.21.0\r\n"),
            Some("v24.21.0".to_owned())
        );
        assert_eq!(parse_nodejs_version(b"24.21.0\n"), None);
        assert_eq!(parse_nodejs_version(b"node v24.21.0\n"), None);
        assert_eq!(parse_nodejs_version(b"v0.0.0\n"), None);
    }

    #[test]
    fn test_nodejs_installer_matches_windows_architecture() {
        let x64 = nodejs_installer_for_architecture("x86_64").expect("x64 installer");
        assert_eq!(x64.version, NODEJS_LTS_VERSION);
        assert_eq!(x64.url, NODEJS_LTS_X64_MSI_URL);
        assert_eq!(x64.sha256, NODEJS_LTS_X64_MSI_SHA256);

        let arm64 = nodejs_installer_for_architecture("aarch64").expect("arm64 installer");
        assert_eq!(arm64.version, NODEJS_LTS_VERSION);
        assert_eq!(arm64.url, NODEJS_LTS_ARM64_MSI_URL);
        assert_eq!(arm64.sha256, NODEJS_LTS_ARM64_MSI_SHA256);

        let x86 = nodejs_installer_for_architecture("x86").expect("x86 installer");
        assert_eq!(x86.version, NODEJS_LTS_X86_VERSION);
        assert_eq!(x86.url, NODEJS_LTS_X86_MSI_URL);
        assert_eq!(x86.sha256, NODEJS_LTS_X86_MSI_SHA256);

        assert!(nodejs_installer_for_architecture("mips").is_err());
    }

    #[test]
    fn test_nodejs_probe_never_resolves_a_bare_executable_name() {
        assert!(probe_node(Path::new("node")).is_none());
    }

    #[test]
    fn test_secure_installer_directory_name_uses_full_random_hex() {
        let suffix = secure_directory_suffix();

        assert_eq!(
            suffix.len(),
            NODEJS_SECURE_INSTALLER_DIRECTORY_RANDOM_BYTES * 2
        );
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn test_secure_installer_directory_dacl_is_protected() {
        assert!(NODEJS_SECURE_INSTALLER_DIRECTORY_SDDL.starts_with("D:P"));
        assert!(NODEJS_SECURE_INSTALLER_DIRECTORY_SDDL.contains("WDWO;;;OW"));
        assert!(NODEJS_SECURE_INSTALLER_DIRECTORY_SDDL.contains("FA;;;SY"));
        assert!(NODEJS_SECURE_INSTALLER_DIRECTORY_SDDL.contains("FA;;;BA"));
    }

    #[test]
    fn test_secure_installer_directory_dacl_is_valid_sddl() {
        let mut sddl = NODEJS_SECURE_INSTALLER_DIRECTORY_SDDL
            .encode_utf16()
            .collect::<Vec<_>>();
        sddl.push(0);
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();

        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                u32::from(SDDL_REVISION_1),
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0, "{}", io::Error::last_os_error());
        unsafe {
            LocalFree(descriptor);
        }
    }

    #[test]
    fn test_nodejs_msi_arguments_are_silent_and_do_not_restart() {
        let args = nodejs_msi_args(Path::new(r"C:\temp\node.msi"));
        let values = args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(values, ["/i", r"C:\temp\node.msi", "/qn", "/norestart"]);
    }
}
