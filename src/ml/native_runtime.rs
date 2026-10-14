//! Load only the hash-pinned official CPU runtime, before any model/native API.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MANIFEST: &str = include_str!("../../scripts/onnx_runtime_manifest.json");
static INITIALIZATION: Mutex<()> = Mutex::new(());
static LOADED_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Deserialize)]
struct Manifest {
    version: String,
    git_commit: String,
    targets: BTreeMap<String, NativeArtifact>,
}

#[derive(Deserialize)]
struct NativeArtifact {
    library_name: String,
    library_sha256: String,
    library_bytes: u64,
    build_revision: String,
    #[serde(default)]
    extra_libraries: Vec<ExtraLibrary>,
}

#[derive(Deserialize)]
struct ExtraLibrary {
    name: String,
    sha256: String,
    bytes: u64,
}

fn target_name() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn runtime_path(manifest: &Manifest, target: &str, artifact: &NativeArtifact) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MACBOT_ONNX_RUNTIME_LIB") {
        anyhow::ensure!(!path.is_empty(), "MACBOT_ONNX_RUNTIME_LIB is empty");
        return Ok(PathBuf::from(path));
    }
    let cache = if let Some(cache) = std::env::var_os("MACBOT_ONNX_RUNTIME_CACHE") {
        anyhow::ensure!(!cache.is_empty(), "MACBOT_ONNX_RUNTIME_CACHE is empty");
        PathBuf::from(cache)
    } else {
        let home =
            std::env::var_os("HOME").context("HOME is missing; set MACBOT_ONNX_RUNTIME_CACHE")?;
        if cfg!(target_os = "macos") {
            PathBuf::from(home).join("Library/Caches/macbot")
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(home).join(".cache"))
                .join("macbot")
        }
    };
    Ok(cache
        .join("onnxruntime")
        .join(&manifest.version)
        .join(target)
        .join(&artifact.library_name))
}

#[cfg(unix)]
fn require_current_owner(owner: u32, current: u32) -> Result<()> {
    anyhow::ensure!(
        owner == current,
        "ONNX runtime file and its directory must be owned by the current user"
    );
    Ok(())
}

#[cfg(unix)]
fn require_trusted_ancestor(owner: u32, current: u32, mode: u32) -> Result<()> {
    anyhow::ensure!(
        owner == current || owner == 0,
        "ONNX runtime ancestor is owned by another user"
    );
    let protected_temporary_root = owner == 0 && mode & 0o1000 != 0;
    anyhow::ensure!(
        mode & 0o022 == 0 || protected_temporary_root,
        "ONNX runtime ancestor is writable by other users"
    );
    Ok(())
}

fn trusted_path(path: &Path) -> Result<PathBuf> {
    let original = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "Pinned ONNX runtime is missing at {}; run python3 scripts/setup_onnx_runtime.py",
            path.display()
        )
    })?;
    anyhow::ensure!(
        original.file_type().is_file(),
        "ONNX runtime must be a regular file, without symbolic links"
    );
    // Resolve ancestor aliases once, then use this exact absolute path for
    // hashing and loading. A later change to an input-path symlink cannot
    // redirect native loading after verification.
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // geteuid has no preconditions; effective UID governs this process's
        // filesystem access, including when launched through sudo.
        let current = unsafe { libc::geteuid() };
        let metadata = std::fs::symlink_metadata(&canonical)?;
        require_current_owner(metadata.uid(), current)?;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o022 == 0,
            "ONNX runtime must not be writable by other users"
        );
        let parent = canonical
            .parent()
            .context("ONNX runtime has no parent directory")?;
        require_current_owner(std::fs::symlink_metadata(parent)?.uid(), current)?;
        for ancestor in parent.ancestors() {
            let metadata = std::fs::symlink_metadata(ancestor)?;
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "ONNX runtime ancestor must be a real directory"
            );
            let mode = metadata.permissions().mode();
            // Root-owned sticky /tmp prevents other users from replacing an
            // owned cache directory. Other shared writable ancestors do not.
            require_trusted_ancestor(metadata.uid(), current, mode)
                .with_context(|| format!("Unsafe ONNX runtime ancestor: {}", ancestor.display()))?;
        }
    }
    Ok(canonical)
}

fn verify_library(path: &Path, bytes: u64, expected_hash: &str) -> Result<PathBuf> {
    let canonical = trusted_path(path)?;
    let metadata = std::fs::symlink_metadata(&canonical)?;
    anyhow::ensure!(
        metadata.len() == bytes,
        "ONNX runtime size differs from the pinned official artifact"
    );
    let mut file = std::fs::File::open(&canonical)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    anyhow::ensure!(
        hex::encode(digest.finalize()) == expected_hash,
        "ONNX runtime fails the pinned official SHA256 check"
    );
    Ok(canonical)
}

pub(super) fn initialize() -> Result<()> {
    let _guard = INITIALIZATION
        .lock()
        .map_err(|_| anyhow::anyhow!("ONNX initialization lock poisoned"))?;
    let manifest: Manifest = serde_json::from_str(MANIFEST)?;
    let target = target_name();
    let artifact = manifest
        .targets
        .get(&target)
        .with_context(|| format!("No pinned CPU ONNX runtime is provided for {target}"))?;
    let selected = runtime_path(&manifest, &target, artifact)?;
    let path = if selected.is_absolute() {
        selected
    } else {
        std::env::current_dir()?.join(selected)
    };
    if let Some(loaded) = LOADED_PATH.get() {
        anyhow::ensure!(
            loaded == &trusted_path(&path)?,
            "ONNX runtime path cannot change after initialization"
        );
        return Ok(());
    }
    let path = verify_library(&path, artifact.library_bytes, &artifact.library_sha256)?;
    let directory = path
        .parent()
        .context("ONNX runtime has no parent directory")?;
    for extra in &artifact.extra_libraries {
        verify_library(&directory.join(&extra.name), extra.bytes, &extra.sha256)?;
    }
    // Native execution begins only after exact artifact verification. The cache
    // is private to the process user; a hostile process with that same user's
    // write access is outside this protection (hash/load is not a sandbox).
    let inspected =
        unsafe { libloading::Library::new(&path) }.context("Cannot load pinned ONNX runtime")?;
    let (version, api_available) = unsafe {
        let getter: libloading::Symbol<unsafe extern "system" fn() -> *const ort::sys::OrtApiBase> =
            inspected.get(b"OrtGetApiBase")?;
        let base = getter();
        anyhow::ensure!(!base.is_null(), "ONNX runtime returned a null API base");
        let version = ((*base).GetVersionString)();
        anyhow::ensure!(!version.is_null(), "ONNX runtime returned a null version");
        (
            CStr::from_ptr(version).to_str()?.to_owned(),
            !((*base).GetApi)(ort::MINOR_VERSION).is_null(),
        )
    };
    anyhow::ensure!(
        version == manifest.version,
        "Expected ONNX Runtime {}, received {version}",
        manifest.version
    );
    anyhow::ensure!(
        api_available,
        "Pinned ONNX runtime does not support binding API {}",
        ort::MINOR_VERSION
    );
    ort::init_from(&path)
        .context("Cannot initialize pinned ONNX runtime")?
        .with_telemetry(false)
        .commit();
    let build_info = ort::info();
    let reported_revision = build_info
        .split("git-commit-id=")
        .nth(1)
        .and_then(|value| value.split([',', ' ']).next());
    anyhow::ensure!(
        artifact.build_revision.len() >= 7
            && manifest.git_commit.starts_with(&artifact.build_revision),
        "Pinned ONNX revision metadata is inconsistent"
    );
    anyhow::ensure!(
        reported_revision == Some(artifact.build_revision.as_str()),
        "Active ONNX runtime build does not match the pinned official revision: {build_info}"
    );
    let _ = LOADED_PATH.set(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_changed_native_bytes_before_loading() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "macbot-native-check-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = directory.join("not-a-runtime");
        std::fs::write(&path, b"original")?;
        let original_hash = hex::encode(Sha256::digest(b"original"));
        verify_library(&path, 8, &original_hash)?;
        std::fs::write(&path, b"modified")?;
        assert!(verify_library(&path, 8, &original_hash).is_err());
        std::fs::remove_file(&path)?;
        std::fs::remove_dir(directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_native_symlinks_before_loading() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "macbot-native-link-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&directory)?;
        let path = directory.join("not-a-runtime");
        std::os::unix::fs::symlink("missing", &path)?;
        assert!(verify_library(&path, 0, "").is_err());
        std::fs::remove_file(path)?;
        std::fs::remove_dir(directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn refuses_replaceable_ancestors_and_foreign_owners() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "macbot-native-ancestor-{:032x}",
            rand::random::<u128>()
        ));
        let parent = directory.join("private-cache");
        std::fs::create_dir_all(&parent)?;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let path = parent.join("runtime");
        std::fs::write(&path, b"native")?;
        let expected = hex::encode(Sha256::digest(b"native"));
        verify_library(&path, 6, &expected)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777))?;
        assert!(
            verify_library(&path, 6, &expected).is_err(),
            "replaceable ancestor accepted"
        );
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o1777))?;
        if unsafe { libc::geteuid() } != 0 {
            assert!(
                verify_library(&path, 6, &expected).is_err(),
                "non-root sticky ancestor accepted"
            );
        } else {
            verify_library(&path, 6, &expected)?;
        }
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        assert!(
            verify_library(&path, 6, &expected).is_err(),
            "writable native file accepted"
        );
        let current = unsafe { libc::geteuid() };
        let foreign = current.wrapping_add(1).max(1);
        assert!(require_current_owner(foreign, current).is_err());
        assert!(
            require_trusted_ancestor(foreign, current, 0o755).is_err(),
            "foreign owner can replace a descendant even without group write"
        );
        std::fs::remove_file(path)?;
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_alias_supports_repeated_initialization_in_a_child_process() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let manifest: Manifest = serde_json::from_str(MANIFEST)?;
        let target = target_name();
        let artifact = manifest.targets.get(&target).context("No pinned target")?;
        let actual = std::fs::canonicalize(runtime_path(&manifest, &target, artifact)?)?;
        let directory = std::env::temp_dir().join(format!(
            "macbot-native-alias-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let alias = directory.join("runtime-directory");
        std::os::unix::fs::symlink(actual.parent().context("Runtime parent missing")?, &alias)?;
        let selected = alias.join(actual.file_name().context("Runtime filename missing")?);
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "ml::deep_model::native_runtime::tests::pinned_runtime_initializes_the_actual_binding", "--nocapture"])
            .env("MACBOT_ONNX_RUNTIME_LIB", selected)
            .output()?;
        std::fs::remove_file(alias)?;
        std::fs::remove_dir(directory)?;
        anyhow::ensure!(
            output.status.success(),
            "Repeated alias initialization failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        anyhow::ensure!(
            String::from_utf8_lossy(&output.stdout)
                .contains("MACBOT_PINNED_NATIVE_INITIALIZED_TWICE"),
            "Native alias child did not execute the expected initialization regression"
        );
        Ok(())
    }

    #[test]
    fn pinned_runtime_initializes_the_actual_binding() -> Result<()> {
        initialize()?;
        initialize()?;
        let manifest: Manifest = serde_json::from_str(MANIFEST)?;
        let artifact = manifest.targets.get(&target_name()).unwrap();
        assert!(ort::info().contains(&format!("git-commit-id={}", artifact.build_revision)));
        println!("MACBOT_PINNED_NATIVE_INITIALIZED_TWICE");
        Ok(())
    }
}
