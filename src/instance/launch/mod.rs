// builds the full java command line and spawns minecraft as a child process.
// handles classpath assembly, auth token injection, and log capture.
// loader-specific patches live in submodules (e.g. patches.rs for lwjgl3ify).

mod patches;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::auth::{Account, AccountType};
use crate::instance::models::{InstanceConfig, ModLoader};

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("Version metadata not found: {0}. Re-create the instance to fix this.")]
    MetaNotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0} launch is not yet supported")]
    NotSupported(String),
    #[error("{0}")]
    Auth(String),
}

// subset of mojang's version meta json, only the bits relevant to launching
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaJson {
    main_class: String,
    asset_index: MetaAssetIndex,
    libraries: Vec<MetaLibrary>,
}

#[derive(serde::Deserialize)]
struct MetaAssetIndex {
    id: String,
}

#[derive(serde::Deserialize)]
struct MetaLibrary {
    downloads: Option<MetaLibraryDownloads>,
    rules: Option<Vec<MetaRule>>,
}

#[derive(serde::Deserialize)]
struct MetaLibraryDownloads {
    artifact: Option<MetaArtifact>,
}

#[derive(serde::Deserialize)]
struct MetaArtifact {
    path: String,
}

#[derive(serde::Deserialize)]
struct MetaRule {
    action: String,
    os: Option<MetaOsRule>,
}

#[derive(serde::Deserialize)]
struct MetaOsRule {
    name: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoaderProfileJson {
    main_class: String,
    libraries: Vec<LoaderLibrary>,
    /// Legacy flat format: `"gameArguments": [...]`
    #[serde(default)]
    game_arguments: Vec<String>,
    /// Forge installer format: `"jvmArguments": [...]`
    #[serde(default)]
    jvm_arguments: Vec<String>,
    /// Modern format: `"arguments": { "game": [...], "jvm": [...] }`
    #[serde(default)]
    arguments: Option<LoaderArguments>,
}

#[derive(serde::Deserialize, Default)]
struct LoaderArguments {
    #[serde(default)]
    game: Vec<String>,
    #[serde(default)]
    jvm: Vec<String>,
}

#[derive(serde::Deserialize)]
struct LoaderLibrary {
    name: String,
}

struct GameAuth {
    username: String,
    uuid: String,
    token: String,
    user_type: String,
}

fn account_can_launch(has_microsoft_account: bool, account: &Account) -> bool {
    account.account_type == AccountType::Microsoft || has_microsoft_account
}

// mojang's library rules are a fun little state machine: each rule can allow
// or disallow based on OS. if no rule matches the current OS, the library is
// included only if no rule "dominated" (matched at all). yes, it's weird.
fn lib_allowed(lib: &MetaLibrary) -> bool {
    let Some(rules) = &lib.rules else {
        return true;
    };
    let current_os = match std::env::consts::OS {
        "macos" => "osx",
        other => other,
    };
    let mut dominated = false;
    for rule in rules {
        let matches_os = rule
            .os
            .as_ref()
            .and_then(|os| os.name.as_deref())
            .is_none_or(|n| n == current_os);
        if !matches_os {
            continue;
        }
        dominated = true;
        match rule.action.as_str() {
            "disallow" => return false,
            "allow" => return true,
            _ => {}
        }
    }
    !dominated
}

fn build_game_args(
    config: &InstanceConfig,
    minecraft_dir: &Path,
    meta_dir: &Path,
    asset_index_id: &str,
    auth: GameAuth,
    loader_game_args: Vec<String>,
) -> Vec<String> {
    let mut game_args = vec![
        "--username".to_string(),
        auth.username,
        "--version".to_string(),
        config.game_version.clone(),
        "--gameDir".to_string(),
        minecraft_dir.to_string_lossy().into_owned(),
        "--assetsDir".to_string(),
        meta_dir.join("assets").to_string_lossy().into_owned(),
        "--assetIndex".to_string(),
        asset_index_id.to_string(),
        "--uuid".to_string(),
        auth.uuid,
        "--accessToken".to_string(),
        auth.token,
        "--userProperties".to_string(),
        "{}".to_string(),
        "--userType".to_string(),
        auth.user_type,
    ];
    game_args.extend(loader_game_args);
    game_args
}

/// Returns true if the Minecraft game version is at least the given major.minor.
/// Handles standard release versions like "1.20.1", "1.21", "1.21.4".
/// Non-numeric version strings (snapshots) always return false.
fn is_game_version_at_least(version: &str, major: u32, minor: u32) -> bool {
    let parts: Vec<u32> = version
        .split('.')
        .filter_map(|s| s.parse().ok())
        .collect();
    parts.len() >= 2 && {
        let (v_major, v_minor) = (parts[0], parts[1]);
        v_major > major || (v_major == major && v_minor >= minor)
    }
}

/// Resolve Forge installer placeholders in JVM arguments:
///   ${library_directory} → meta libraries dir
///   ${classpath_separator} → ':' or ';'
///   ${version_name}        → e.g. "1.20.1-forge-47.4.0"
fn resolve_jvm_placeholders(
    args: Vec<String>,
    library_directory: &str,
    classpath_separator: &str,
    version_name: &str,
) -> Vec<String> {
    args.into_iter()
        .map(|arg| {
            arg.replace("${library_directory}", library_directory)
                .replace("${classpath_separator}", classpath_separator)
                .replace("${version_name}", version_name)
        })
        .collect()
}

pub async fn launch(
    config: &InstanceConfig,
    instances_dir: &Path,
    meta_dir: &Path,
) -> Result<(), LaunchError> {
    let name = config.name.clone();
    let instance_dir = instances_dir.join(&name);
    let minecraft_dir = instance_dir.join(".minecraft");

    let meta_path = meta_dir
        .join("versions")
        .join(&config.game_version)
        .join("meta.json");
    if !meta_path.exists() {
        return Err(LaunchError::MetaNotFound(meta_path.display().to_string()));
    }
    let meta: MetaJson = serde_json::from_slice(&tokio::fs::read(&meta_path).await?)?;

    let lib_dir = meta_dir.join("libraries");
    let mut classpath: Vec<PathBuf> = meta
        .libraries
        .iter()
        .filter(|l| lib_allowed(l))
        .filter_map(|l| {
            l.downloads
                .as_ref()?
                .artifact
                .as_ref()
                .map(|a| lib_dir.join(&a.path))
        })
        .collect();

    let lv = config.loader_version.as_deref().unwrap_or("unknown");
    let profile_filename = match config.loader {
        ModLoader::Vanilla => None,
        ModLoader::Fabric => Some(format!("fabric-{}-{}.json", config.game_version, lv)),
        ModLoader::Quilt => Some(format!("quilt-{}-{}.json", config.game_version, lv)),
        ModLoader::Forge => Some(format!("forge-{}-{}.json", config.game_version, lv)),
        ModLoader::NeoForge => Some(format!("neoforge-{}.json", lv)),
    };

    // if there's a mod loader, read its profile to get the real main class,
    // extra libraries, and any additional game arguments (e.g. --tweakClass)
    let (main_class, loader_game_args, loader_jvm_args) = if let Some(filename) = profile_filename {
        let profile_path = meta_dir.join("loader-profiles").join(&filename);
        if !profile_path.exists() {
            return Err(LaunchError::MetaNotFound(
                profile_path.display().to_string(),
            ));
        }
        let mut profile: LoaderProfileJson =
            serde_json::from_slice(&tokio::fs::read(&profile_path).await?)?;

        // forge/neoforge install some libs locally in the instance dir.
        // local libs take priority so modpacks can ship patched versions
        // (e.g. GTNH's launchwrapper patched for java 9+ compatibility)
        let has_local_libs = matches!(config.loader, ModLoader::Forge | ModLoader::NeoForge);
        let local_lib_dir = minecraft_dir.join("libraries");

        for lib in &profile.libraries {
            if let Some(p) = crate::net::maven_coord_to_path(&lib.name) {
                if has_local_libs {
                    let in_local = local_lib_dir.join(&p);
                    let in_meta = lib_dir.join(&p);
                    if in_local.exists() {
                        classpath.push(in_local);
                    } else if in_meta.exists() {
                        classpath.push(in_meta);
                    }
                } else {
                    classpath.push(lib_dir.join(p));
                }
            }
        }
        // merge game args from both modern (arguments.game) and legacy (gameArguments) formats
        let main_class = profile.main_class;
        let game_args_legacy = profile.game_arguments;
        let args = profile.arguments.take();
        let game_args = match args {
            Some(ref a) if !a.game.is_empty() => a.game.clone(),
            _ => game_args_legacy,
        };
        // merge jvm args from modern (arguments.jvm) and forge (jvmArguments) formats.
        // forge installer profiles use the legacy jvmArguments format with ${…} placeholders
        // that must be resolved to actual paths before passing to java.
        let jvm_args_raw = if let Some(ref a) = args {
            if !a.jvm.is_empty() { a.jvm.clone() } else { profile.jvm_arguments }
        } else {
            profile.jvm_arguments
        };
        let version_name = match config.loader {
            ModLoader::Forge => format!("{}-forge-{}", config.game_version, lv),
            ModLoader::NeoForge => format!("neoforge-{}", lv),
            _ => config.game_version.clone(),
        };
        // forge/neoforge installer puts its own libraries in the instance dir,
        // not in the global meta dir. ${library_directory} must resolve there.
        let library_directory = if matches!(config.loader, ModLoader::Forge | ModLoader::NeoForge) {
            minecraft_dir.join("libraries").to_string_lossy().into_owned()
        } else {
            lib_dir.to_string_lossy().into_owned()
        };
        let cp_sep = if cfg!(windows) { ";" } else { ":" };
        let jvm_args = resolve_jvm_placeholders(jvm_args_raw, &library_directory, cp_sep, &version_name);
        (main_class, game_args, jvm_args)
    } else {
        (meta.main_class.clone(), Vec::new(), Vec::new())
    };

    classpath.push(
        meta_dir
            .join("versions")
            .join(&config.game_version)
            .join(format!("{}.jar", config.game_version)),
    );

    // apply loader-specific patches (lwjgl3ify for old forge on java 9+)
    let (patch_jvm_args, main_class, extra_args) = if matches!(config.loader, ModLoader::Forge) {
        match patches::apply(&minecraft_dir, &lib_dir, &mut classpath).await {
            Some(p) => (p.jvm_args, p.main_class, p.extra_args),
            None => (Vec::new(), main_class, Vec::new()),
        }
    } else {
        (Vec::new(), main_class, Vec::new())
    };

    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp_str = classpath
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep);

    // java resolution: instance override > global setting > auto-detect
    let java = config
        .java_path
        .clone()
        .or_else(|| {
            crate::config::SETTINGS
                .paths
                .effective_java_path()
                .map(str::to_owned)
        })
        .unwrap_or_else(crate::net::detect_java_path);

    let mut jvm: Vec<String> = vec![
        format!("-Xms{}", config.memory_min.as_deref().unwrap_or(&crate::config::SETTINGS.defaults.memory_min)),
        format!("-Xmx{}", config.memory_max.as_deref().unwrap_or(&crate::config::SETTINGS.defaults.memory_max)),
    ];
    jvm.extend(loader_jvm_args);
    jvm.extend(patch_jvm_args);
    jvm.extend(config.jvm_args.clone());

    // resolve auth credentials, refreshing the microsoft token if needed.
    let mut account_store = crate::auth::AccountStore::load();
    let (mc_username, mc_uuid, mc_token, mc_user_type) = match account_store
        .active_account()
        .cloned()
    {
        Some(acc) => {
            if !account_can_launch(account_store.has_microsoft_account(), &acc) {
                return Err(LaunchError::Auth(
                    "Offline accounts require a Microsoft account that owns Minecraft".to_owned(),
                ));
            }
            let (token, new_refresh, new_expires) = match acc.account_type {
                AccountType::Microsoft => match crate::auth::refresh_and_get_token(&acc).await {
                    Ok(triple) => triple,
                    Err(e) => {
                        return Err(LaunchError::Auth(format!("Authentication failed: {e}")));
                    }
                },
                AccountType::Offline => ("0".to_string(), None, None),
            };
            if let Some(stored) = account_store
                .accounts
                .iter_mut()
                .find(|a| a.uuid == acc.uuid)
            {
                let mut changed = false;
                if let Some(new_rt) = new_refresh {
                    stored.refresh_token = Some(new_rt);
                    changed = true;
                }
                if let Some(expires) = new_expires {
                    stored.cached_mc_token = Some(token.clone());
                    stored.cached_mc_token_expires_at = Some(expires);
                    changed = true;
                }
                if changed {
                    account_store.save();
                }
            }
            let user_type = match acc.account_type {
                AccountType::Microsoft => "msa",
                AccountType::Offline => "legacy",
            };
            (
                acc.username.clone(),
                acc.uuid.clone(),
                token,
                user_type.to_string(),
            )
        }
        None => return Err(LaunchError::Auth("No account selected".to_owned())),
    };

    let mut game_args = build_game_args(
        config,
        &minecraft_dir,
        meta_dir,
        &meta.asset_index.id,
        GameAuth {
            username: mc_username,
            uuid: mc_uuid,
            token: mc_token,
            user_type: mc_user_type,
        },
        loader_game_args,
    );

    // forgebootstrap (forge 1.21+) needs an explicit launch target,
    // otherwise modlauncher may pass null to immediatewindowhandler.
    // neoForge uses its own launch mechanism and doesn't need this.
    if config.loader == ModLoader::Forge && is_game_version_at_least(&config.game_version, 1, 21) {
        // add at the front so ForgeBootstrap finds it before any game args
        game_args.insert(0, "forge_client".to_string());
        game_args.insert(0, "--launchTarget".to_string());
    }

    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    crate::running::register_kill(&name, kill_tx);
    crate::running::set_state(&name, crate::running::RunState::Starting);
    tracing::info!(
        "[{}] Starting Minecraft ({} {})",
        name,
        config.game_version,
        config.loader
    );

    tracing::info!("[{}] Java: {}", name, java);
    tracing::info!("[{}] JVM args: {:?}", name, jvm);
    tracing::info!(
        "[{}] Classpath:\n{}",
        name,
        classpath
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    tracing::info!("[{}] Main class: {}", name, main_class);

    let mut cmd = tokio::process::Command::new(&java);
    cmd.args(&jvm);
    cmd.arg("-cp").arg(&cp_str);
    cmd.arg(&main_class);
    cmd.args(&extra_args);
    cmd.args(&game_args);
    cmd.current_dir(&minecraft_dir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // linux-only env setup for the child process.
    //
    // strip inherited LD_LIBRARY_PATH/LD_PRELOAD to avoid nvidia's libegl
    // segfaulting when lwjgl uses egl (prismlauncher does the same in
    // CleanEnvironment). users can set LAUNCHER_LD_LIBRARY_PATH /
    // LAUNCHER_LD_PRELOAD to inject specific paths if needed.
    //
    // also force GDK_BACKEND=x11 when running under wayland with xwayland
    // available, to avoid forge early-window (gtk) initialisation issues.
    #[cfg(target_os = "linux")]
    {
        let ld_path = std::env::var("LD_LIBRARY_PATH").ok();
        let launcher_ld = std::env::var("LAUNCHER_LD_LIBRARY_PATH").ok();
        match (ld_path, launcher_ld) {
            (Some(_), Some(override_path)) => {
                cmd.env("LD_LIBRARY_PATH", &override_path);
            }
            (Some(_), None) => {
                cmd.env_remove("LD_LIBRARY_PATH");
            }
            _ => {}
        }

        let ld_preload = std::env::var("LD_PRELOAD").ok();
        let launcher_preload = std::env::var("LAUNCHER_LD_PRELOAD").ok();
        match (ld_preload, launcher_preload) {
            (Some(_), Some(override_path)) => {
                cmd.env("LD_PRELOAD", &override_path);
            }
            (Some(_), None) => {
                cmd.env_remove("LD_PRELOAD");
            }
            _ => {}
        }

        // gdk defaults to wayland backend on wayland, but forge's early
        // loading gtk window has issues with native wayland. force the
        // X11 backend through xwayland when available.
        //
        // also unset WAYLAND_DISPLAY so glfw uses X11/XWayland instead of
        // native wayland, which prevents "platform does not provide window
        // position" crashes on some wayland compositors.
        let is_wayland = std::env::var("XDG_SESSION_TYPE")
            .map(|v| v == "wayland")
            .unwrap_or(false)
            || std::env::var("WAYLAND_DISPLAY").is_ok();
        if is_wayland && std::env::var("DISPLAY").is_ok() {
            cmd.env("GDK_BACKEND", "x11");
            cmd.env_remove("WAYLAND_DISPLAY");
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            crate::running::cleanup_kill_sender(&name);
            crate::running::remove(&name);
            return Err(LaunchError::Io(e));
        }
    };

    crate::running::set_state(&name, crate::running::RunState::Running);

    let log_file_path = crate::instance::log_files::create_log_file(instances_dir, &name);

    let name_for_task = name.clone();
    let instances_dir_owned = instances_dir.to_path_buf();
    let meta_dir_owned = meta_dir.to_path_buf();

    // spawn a background task to babysit the child process: capture stdout/stderr
    // into both the TUI log viewer and a timestamped log file on disk
    tokio::spawn(async move {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tokio::io::AsyncBufReadExt;

        let log_writer: Arc<Mutex<Option<std::fs::File>>> = Arc::new(Mutex::new(
            log_file_path.and_then(|p| std::fs::File::create(p).ok()),
        ));

        if let Some(stdout) = child.stdout.take() {
            let n = name_for_task.clone();
            let w = log_writer.clone();
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!(target: "mc_instance", "[{}] {}", n, line);
                    crate::instance_logs::push(&n, &line);
                    if let Ok(mut f) = w.lock()
                        && let Some(f) = f.as_mut()
                    {
                        let _ = writeln!(f, "{}", line);
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let n = name_for_task.clone();
            let w = log_writer.clone();
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(target: "mc_instance", "[{}] {}", n, line);
                    crate::instance_logs::push(&n, &line);
                    if let Ok(mut f) = w.lock()
                        && let Some(f) = f.as_mut()
                    {
                        let _ = writeln!(f, "[STDERR] {}", line);
                    }
                }
            });
        }

        // wait for either the process to exit naturally or a kill signal from the TUI
        let code = tokio::select! {
            _ = kill_rx => {
                tracing::info!("[{}] Kill requested, terminating process", name_for_task);
                let _ = child.kill().await;
                let _ = child.wait().await;
                None
            }
            result = child.wait() => {
                result.ok().and_then(|s| s.code())
            }
        };
        tracing::info!("[{}] Exited with code {:?}", name_for_task, code);

        if code == Some(0) {
            crate::running::remove(&name_for_task);
        } else {
            crate::running::set_state(&name_for_task, crate::running::RunState::Crashed(code));
        }

        let manager = crate::instance::InstanceManager::new(instances_dir_owned, meta_dir_owned);
        if let Err(e) = manager.touch_last_played(&name_for_task) {
            tracing::warn!(
                "Failed to update last_played for '{}': {}",
                name_for_task,
                e
            );
        }
        crate::running::push_last_played(&name_for_task, chrono::Utc::now());
        crate::running::cleanup_kill_sender(&name_for_task);
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Account, AccountType};
    use chrono::Utc;

    fn test_config() -> InstanceConfig {
        InstanceConfig {
            name: "test".to_owned(),
            game_version: "1.7.10".to_owned(),
            loader: ModLoader::Forge,
            loader_version: Some("10.13.4.1614".to_owned()),
            created: Utc::now(),
            last_played: None,
            java_path: None,
            memory_max: None,
            memory_min: None,
            jvm_args: Vec::new(),
            resolution: None,
        }
    }

    fn test_account(account_type: AccountType) -> Account {
        Account {
            uuid: "00000000-0000-0000-0000-000000000001".to_owned(),
            username: "TestPlayer".to_owned(),
            account_type,
            active: true,
            refresh_token: Some("refresh".to_owned()),
            cached_mc_token: None,
            cached_mc_token_expires_at: None,
        }
    }

    #[test]
    fn game_args_include_empty_user_properties() {
        let args = build_game_args(
            &test_config(),
            Path::new("/instances/test/.minecraft"),
            Path::new("/meta"),
            "legacy",
            GameAuth {
                username: "TestPlayer".to_owned(),
                uuid: "00000000-0000-0000-0000-000000000000".to_owned(),
                token: "token".to_owned(),
                user_type: "msa".to_owned(),
            },
            vec![
                "--tweakClass".to_owned(),
                "cpw.mods.fml.common.launcher.FMLTweaker".to_owned(),
            ],
        );

        let position = args
            .iter()
            .position(|arg| arg == "--userProperties")
            .expect("game args should include --userProperties");
        assert_eq!(args.get(position + 1).map(String::as_str), Some("{}"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--tweakClass", "cpw.mods.fml.common.launcher.FMLTweaker"])
        );
    }

    #[test]
    fn offline_account_cannot_launch_without_microsoft_account() {
        let offline = test_account(AccountType::Offline);

        assert!(!account_can_launch(false, &offline));
    }

    #[test]
    fn offline_account_can_launch_with_microsoft_account() {
        let offline = test_account(AccountType::Offline);

        assert!(account_can_launch(true, &offline));
    }

    #[test]
    fn microsoft_account_can_launch_without_offline_gate() {
        let microsoft = test_account(AccountType::Microsoft);

        assert!(account_can_launch(false, &microsoft));
    }
}
