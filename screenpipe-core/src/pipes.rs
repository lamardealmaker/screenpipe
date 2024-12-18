#[allow(clippy::module_inception)]
#[cfg(feature = "pipes")]
mod pipes {
    use regex::Regex;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::process::Command;

    use reqwest::Client;
    use serde_json::Value;

    use anyhow::Result;
    use reqwest;
    use std::fs;
    use std::path::Path;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tracing::{debug, error, info};
    use url::Url;
    use which::which;

    // Add these imports at the top of the file
    use serde_json::json;
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;

    use crate::pick_unused_port;
    use once_cell::sync::Lazy;

    // Add near other imports
    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
    use std::str::FromStr;

    // Add this function to generate a secure cron secret
    fn generate_cron_secret() -> String {
        thread_rng()
            .sample_iter(&Alphanumeric)
            .take(32)
            .map(char::from)
            .collect()
    }

    // Update this function near the top of the file
    fn sanitize_pipe_name(name: &str) -> String {
        let re = Regex::new(r"[^a-zA-Z0-9_-]").unwrap();
        let sanitized = re.replace_all(name, "-").to_string();

        // Remove "-ref-main/" suffix if it exists
        sanitized
            .strip_suffix("-ref-main/")
            .or_else(|| sanitized.strip_suffix("-ref-main"))
            .unwrap_or(&sanitized)
            .to_string()
    }

    pub async fn run_pipe(pipe: &str, screenpipe_dir: PathBuf) -> Result<tokio::process::Child> {
        let bun_path = find_bun_path().ok_or_else(|| anyhow::anyhow!("bun not found"))?;
        let pipe_dir = screenpipe_dir.join("pipes").join(pipe);
        let pipe_json_path = pipe_dir.join("pipe.json");

        // Check if pipe is still enabled
        if pipe_json_path.exists() {
            let pipe_json = tokio::fs::read_to_string(&pipe_json_path).await?;
            let pipe_config: Value = serde_json::from_str(&pipe_json)?;

            if !pipe_config
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                debug!("pipe {} is disabled, stopping", pipe);
                anyhow::bail!("pipe is disabled");
            }
        }

        // Prepare environment variables
        let mut env_vars = std::env::vars().collect::<Vec<(String, String)>>();
        env_vars.push((
            "SCREENPIPE_DIR".to_string(),
            screenpipe_dir.to_str().unwrap().to_string(),
        ));
        env_vars.push(("PIPE_ID".to_string(), pipe.to_string()));
        env_vars.push((
            "PIPE_DIR".to_string(),
            pipe_dir.to_str().unwrap().to_string(),
        ));

        if pipe_json_path.exists() {
            let pipe_json = tokio::fs::read_to_string(&pipe_json_path).await?;
            let pipe_config: Value = serde_json::from_str(&pipe_json)?;

            if pipe_config["is_nextjs"] == json!(true) {
                // Handle cron jobs if they exist
                if let Some(crons) = pipe_config.get("crons").and_then(Value::as_array) {
                    let port = pipe_config["port"].as_u64().unwrap_or(3000) as u16;
                    let base_url = format!("http://localhost:{}", port);

                    // Generate a CRON_SECRET if not exists
                    let cron_secret = generate_cron_secret();

                    // Add CRON_SECRET to environment variables
                    env_vars.push(("CRON_SECRET".to_string(), cron_secret.clone()));

                    // Clone the crons array before the loop
                    let crons = crons.to_vec();

                    for cron in crons {
                        let path = cron["path"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("missing path"))?
                            .to_string();
                        let schedule = cron["schedule"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("missing schedule"))?
                            .to_string();

                        // Validate cron expression
                        cron::Schedule::from_str(&schedule)
                            .map_err(|e| anyhow::anyhow!("invalid cron: {}", e))?;

                        let base_url = base_url.clone();
                        let pipe_clone = pipe.to_string();
                        let secret_clone = cron_secret.clone();

                        tokio::spawn(async move {
                            run_cron_schedule(
                                &pipe_clone,
                                &base_url,
                                &path,
                                &secret_clone,
                                &schedule,
                            )
                            .await;
                        });
                    }
                }

                // Install dependencies using bun
                info!("installing dependencies for next.js pipe");
                let install_output = Command::new(&bun_path)
                    .arg("install")
                    .current_dir(&pipe_dir)
                    .output()
                    .await?;

                if !install_output.status.success() {
                    error!(
                        "failed to install dependencies: {}",
                        String::from_utf8_lossy(&install_output.stderr)
                    );
                    anyhow::bail!("failed to install dependencies for next.js pipe");
                }

                let port = pick_unused_port().expect("No ports free");

                // Update pipe.json with the port
                let mut updated_config = pipe_config.clone();
                updated_config["port"] = json!(port);
                let updated_pipe_json = serde_json::to_string_pretty(&updated_config)?;
                let mut file = File::create(&pipe_json_path).await?;
                file.write_all(updated_pipe_json.as_bytes()).await?;

                env_vars.push(("PORT".to_string(), port.to_string()));

                // Run the Next.js project with bun
                let mut child = Command::new(&bun_path)
                    .arg("run")
                    .arg("dev")
                    .arg("--port")
                    .arg(port.to_string())
                    .current_dir(&pipe_dir)
                    .envs(env_vars)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()?;

                // Stream logs
                stream_logs(pipe, &mut child).await?;

                return Ok(child);
            }
        }

        // If it's not a Next.js project, run the pipe as before
        let main_module = find_pipe_file(&pipe_dir)?;
        info!("executing pipe: {:?}", main_module);

        // Add PIPE_FILE to environment variables for non-Next.js pipes
        env_vars.push((
            "PIPE_FILE".to_string(),
            main_module.to_str().unwrap().to_string(),
        ));

        let mut child = Command::new(&bun_path)
            .arg("run")
            .arg(&main_module)
            .envs(env_vars)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        // Stream logs - don't block the main thread
        stream_logs(pipe, &mut child).await?;

        Ok(child)
    }

    async fn stream_logs(pipe: &str, child: &mut tokio::process::Child) -> Result<()> {
        let stdout = child.stdout.take().expect("failed to get stdout");
        let stderr = child.stderr.take().expect("failed to get stderr");

        let pipe_clone = pipe.to_string();

        // Spawn tasks to handle stdout and stderr
        let _stdout_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                info!("[{}] {}", pipe_clone, line);
            }
        });

        let pipe_clone = pipe.to_string();

        let _stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // List of patterns that should be treated as info logs
                let info_patterns = [
                    "Download",
                    "Task dev ",
                    "$ next dev",
                    "ready started server",
                    "Local:",
                    "Webpack is configured",
                    "See instructions",
                    "https://nextjs.org",
                ];

                if info_patterns.iter().any(|pattern| line.contains(pattern)) {
                    info!("[{}] {}", pipe_clone, line);
                } else {
                    error!("[{}] {}", pipe_clone, line);
                }
            }
        });

        info!("pipe execution completed successfully");
        Ok(())
    }

    pub async fn download_pipe(source: &str, screenpipe_dir: PathBuf) -> anyhow::Result<PathBuf> {
        info!("Processing pipe from source: {}", source);

        let pipe_name =
            sanitize_pipe_name(Path::new(source).file_name().unwrap().to_str().unwrap());
        let dest_dir = screenpipe_dir.join("pipes").join(&pipe_name);

        debug!("Destination directory: {:?}", dest_dir);

        // Save existing pipe.json content before downloading
        let pipe_json_path = dest_dir.join("pipe.json");
        let existing_config = if pipe_json_path.exists() {
            debug!("Existing pipe.json found");
            let content = tokio::fs::read_to_string(&pipe_json_path).await?;
            Some(serde_json::from_str::<Value>(&content)?)
        } else {
            debug!("No existing pipe.json found");
            None
        };

        // Create temp directory for download
        let temp_dir = dest_dir.with_extension("_temp");
        tokio::fs::create_dir_all(&temp_dir).await?;

        // Download to temp directory first
        let download_result = if let Ok(parsed_url) = Url::parse(source) {
            debug!("Source is a URL: {}", parsed_url);
            if parsed_url.host_str() == Some("github.com") {
                download_github_folder(&parsed_url, &temp_dir).await
            } else {
                anyhow::bail!("Unsupported URL format");
            }
        } else {
            debug!("Source is a local path");
            let source_path = Path::new(source);
            if !source_path.exists() || !source_path.is_dir() {
                anyhow::bail!("Invalid local source path");
            }
            copy_dir_all(source_path, &temp_dir).await
        };

        // remove temp dir if download failed
        if let Err(e) = download_result {
            tokio::fs::remove_dir_all(&temp_dir).await?;
            error!("Failed to download pipe: {}", e);
        }

        // If download successful, move temp dir to final location
        if dest_dir.exists() {
            tokio::fs::remove_dir_all(&dest_dir).await?;
        }
        tokio::fs::rename(&temp_dir, &dest_dir).await?;

        // Restore or merge pipe.json if needed
        if let Some(ref existing_config) = existing_config {
            let new_config_path = dest_dir.join("pipe.json");
            if new_config_path.exists() {
                let content = tokio::fs::read_to_string(&new_config_path).await?;
                let new_json: Value = serde_json::from_str(&content)?;

                // Create merged config
                let mut merged_config = new_json.clone(); // Start with new schema

                // If both configs have fields array, preserve user values
                if let (Some(existing_obj), Some(new_obj)) =
                    (existing_config.as_object(), merged_config.as_object_mut())
                {
                    // Copy over non-fields properties from existing config
                    for (key, value) in existing_obj {
                        if key != "fields" {
                            new_obj.insert(key.clone(), value.clone());
                        }
                    }

                    // For fields array, preserve user values while keeping new schema
                    if let (Some(existing_fields), Some(new_fields)) = (
                        existing_config["fields"].as_array(),
                        new_obj.get_mut("fields").and_then(|f| f.as_array_mut()),
                    ) {
                        // For each field in the new schema
                        for new_field in new_fields {
                            if let Some(name) = new_field.get("name").and_then(Value::as_str) {
                                // If this field existed in the old config, preserve its value
                                if let Some(existing_field) = existing_fields
                                    .iter()
                                    .find(|f| f.get("name").and_then(Value::as_str) == Some(name))
                                {
                                    if let Some(user_value) = existing_field.get("value") {
                                        if let Some(new_field_obj) = new_field.as_object_mut() {
                                            new_field_obj
                                                .insert("value".to_string(), user_value.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                let config_str = serde_json::to_string_pretty(&merged_config)?;
                tokio::fs::write(&new_config_path, config_str).await?;
            } else {
                // If no new config exists, keep the existing one
                let config_str = serde_json::to_string_pretty(&existing_config)?;
                tokio::fs::write(&new_config_path, config_str).await?;
            }
        }

        // After downloading/copying the pipe, check if it's a Next.js project
        let package_json_path = dest_dir.join("package.json");
        if package_json_path.exists() {
            let package_json = tokio::fs::read_to_string(&package_json_path).await?;
            let package_data: Value = serde_json::from_str(&package_json)?;

            let bun_path = find_bun_path().ok_or_else(|| anyhow::anyhow!("bun not found"))?;

            // Make bun install mandatory for all package.json pipes
            let mut install_child = Command::new(&bun_path)
                .arg("i")
                .current_dir(&dest_dir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;

            // Stream logs for npm install
            stream_logs("bun install", &mut install_child).await?;

            if package_data["dependencies"].get("next").is_some() {
                info!("Detected Next.js project, setting up for production");
                // Update pipe.json to indicate it's a Next.js project
                let mut pipe_config = if let Some(existing_json) = &existing_config {
                    existing_json.clone()
                } else if pipe_json_path.exists() {
                    let pipe_json = tokio::fs::read_to_string(&pipe_json_path).await?;
                    serde_json::from_str(&pipe_json)?
                } else {
                    json!({})
                };

                pipe_config["is_nextjs"] = json!(true);
                let updated_pipe_json = serde_json::to_string_pretty(&pipe_config)?;
                let mut file = File::create(&pipe_json_path).await?;
                file.write_all(updated_pipe_json.as_bytes()).await?;
            }
        }

        info!("pipe copied successfully to: {:?}", dest_dir);
        Ok(dest_dir)
    }

    async fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> anyhow::Result<()> {
        let src = src.as_ref();
        let dst = dst.as_ref();
        debug!("copy_dir_all: src={:?}, dst={:?}", src, dst);

        tokio::fs::create_dir_all(&dst).await?;
        debug!("Created destination directory: {:?}", dst);

        let mut entries = tokio::fs::read_dir(src).await?;
        debug!("Reading source directory: {:?}", src);

        while let Some(entry) = entries.next_entry().await? {
            let ty = entry.file_type().await?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            debug!("Processing entry: {:?}", src_path);

            if should_ignore(&entry.file_name()) {
                debug!("Skipping ignored file/directory: {:?}", entry.file_name());
                continue;
            }

            if ty.is_dir() {
                debug!("Entry is a directory, recursing: {:?}", src_path);
                copy_dir_all_boxed(src_path, dst_path).await?;
            } else {
                debug!("Copying file: {:?} to {:?}", src_path, dst_path);
                tokio::fs::copy(&src_path, &dst_path).await?;
            }
        }

        debug!("Finished copying directory: {:?}", src);
        Ok(())
    }

    fn copy_dir_all_boxed(
        src: impl AsRef<Path> + Send + 'static,
        dst: impl AsRef<Path> + Send + 'static,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(copy_dir_all(src, dst))
    }

    fn should_ignore(file_name: &std::ffi::OsStr) -> bool {
        let ignore_list = [
            "node_modules",
            ".git",
            ".next",
            "dist",
            "build",
            ".DS_Store",
            "Thumbs.db",
            ".env",
            ".env.local",
            ".env.development.local",
            ".env.test.local",
            ".env.production.local",
        ];

        ignore_list.iter().any(|ignored| file_name == *ignored)
            || file_name.to_str().map_or(false, |s| s.starts_with('.'))
    }

    fn download_github_folder(
        url: &Url,
        dest_dir: &Path,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        let url = url.clone();
        let dest_dir = dest_dir.to_path_buf();

        Box::pin(async move {
            let client = Client::new();
            let api_url = get_raw_github_url(url.as_str())?;

            let response = client
                .get(&api_url)
                .header("Accept", "application/vnd.github.v3+json")
                .header("User-Agent", "screenpipe")
                .send()
                .await?;

            let contents: Value = response.json().await?;

            if !contents.is_array() {
                anyhow::bail!("invalid response from github api");
            }

            for item in contents.as_array().unwrap() {
                let file_name = item["name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing name in github response"))?;

                if !is_hidden_file(std::ffi::OsStr::new(file_name)) {
                    let file_type = item["type"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("missing type in github response"))?;

                    match file_type {
                        "file" => {
                            let download_url = item["download_url"].as_str().ok_or_else(|| {
                                anyhow::anyhow!("missing download_url in github response")
                            })?;

                            let file_content =
                                client.get(download_url).send().await?.bytes().await?;
                            let file_path = dest_dir.join(file_name);
                            tokio::fs::write(&file_path, &file_content).await?;
                            debug!("downloaded file: {:?}", file_path);
                        }
                        "dir" => {
                            let new_dest_dir = dest_dir.join(file_name);
                            tokio::fs::create_dir_all(&new_dest_dir).await?;

                            let new_url = format!("{}/{}", url.as_str(), file_name);
                            download_github_folder(&Url::parse(&new_url)?, &new_dest_dir).await?;
                            debug!("downloaded directory: {:?}", new_dest_dir);
                        }
                        _ => debug!("skipping unknown type: {}", file_type),
                    }
                } else {
                    debug!("skipping hidden file: {}", file_name);
                }
            }

            Ok(())
        })
    }

    fn get_raw_github_url(url: &str) -> anyhow::Result<String> {
        info!("Attempting to get raw GitHub URL for: {}", url);
        let parsed_url = Url::parse(url)?;
        if parsed_url.host_str() == Some("github.com") {
            let path_segments: Vec<&str> = parsed_url.path_segments().unwrap().collect();
            if path_segments.len() >= 5 && path_segments[2] == "tree" {
                let (owner, repo, _, branch) = (
                    path_segments[0],
                    path_segments[1],
                    path_segments[2],
                    path_segments[3],
                );
                let raw_path = path_segments[4..].join("/");
                let raw_url = format!(
                    "https://api.github.com/repos/{}/{}/contents/{}?ref={}",
                    owner, repo, raw_path, branch
                );
                info!("Converted to GitHub API URL: {}", raw_url);
                return Ok(raw_url);
            }
        }
        anyhow::bail!("Invalid GitHub URL format")
    }

    fn find_pipe_file(pipe_dir: &Path) -> anyhow::Result<PathBuf> {
        for entry in fs::read_dir(pipe_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let file_name_str = file_name.to_str().unwrap();
            if (file_name_str == "pipe.js" || file_name_str == "pipe.ts")
                && !is_hidden_file(&file_name)
            {
                return Ok(entry.path());
            }
        }
        anyhow::bail!("No pipe.js/pipe.ts found in the pipe/dist directory")
    }

    fn is_hidden_file(file_name: &std::ffi::OsStr) -> bool {
        file_name
            .to_str()
            .map(|s| s.starts_with('.') || s == "Thumbs.db")
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    const BUN_EXECUTABLE_NAME: &str = "bun";

    #[cfg(windows)]
    const BUN_EXECUTABLE_NAME: &str = "bun.exe";

    static BUN_PATH: Lazy<Option<PathBuf>> = Lazy::new(find_bun_path_internal);

    pub fn find_bun_path() -> Option<PathBuf> {
        BUN_PATH.as_ref().map(|p| p.clone())
    }

    fn find_bun_path_internal() -> Option<PathBuf> {
        debug!("starting search for bun executable");

        // Check if bun is in PATH
        if let Ok(path) = which(BUN_EXECUTABLE_NAME) {
            debug!("found bun in PATH: {:?}", path);
            return Some(path);
        }
        debug!("bun not found in PATH");

        // Check in current working directory
        if let Ok(cwd) = std::env::current_dir() {
            debug!("current working directory: {:?}", cwd);
            let bun_in_cwd = cwd.join(BUN_EXECUTABLE_NAME);
            if bun_in_cwd.is_file() && bun_in_cwd.exists() {
                debug!("found bun in current working directory: {:?}", bun_in_cwd);
                return Some(bun_in_cwd);
            }
            debug!("bun not found in current working directory");
        }

        // Check in executable directory
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_folder) = exe_path.parent() {
                debug!("executable folder: {:?}", exe_folder);
                let bun_in_exe_folder = exe_folder.join(BUN_EXECUTABLE_NAME);
                if bun_in_exe_folder.exists() {
                    debug!("found bun in executable folder: {:?}", bun_in_exe_folder);
                    return Some(bun_in_exe_folder);
                }
                debug!("bun not found in executable folder");

                // Platform-specific checks
                #[cfg(target_os = "macos")]
                {
                    let resources_folder = exe_folder.join("../Resources");
                    debug!("resources folder: {:?}", resources_folder);
                    let bun_in_resources = resources_folder.join(BUN_EXECUTABLE_NAME);
                    if bun_in_resources.exists() {
                        debug!("found bun in resources folder: {:?}", bun_in_resources);
                        return Some(bun_in_resources);
                    }
                    debug!("bun not found in resources folder");
                }

                #[cfg(target_os = "linux")]
                {
                    let lib_folder = exe_folder.join("lib");
                    debug!("lib folder: {:?}", lib_folder);
                    let bun_in_lib = lib_folder.join(BUN_EXECUTABLE_NAME);
                    if bun_in_lib.exists() {
                        debug!("found bun in lib folder: {:?}", bun_in_lib);
                        return Some(bun_in_lib);
                    }
                    debug!("bun not found in lib folder");
                }
            }
        }

        error!("bun not found");
        None
    }

    // Add this function to handle cron state persistence
    pub async fn get_last_cron_execution(pipe_dir: &Path, path: &str) -> Result<Option<SystemTime>> {
        let state_file = pipe_dir.join(".cron_state.json");

        if !state_file.exists() {
            return Ok(None);
        }

        let content = tokio::fs::read_to_string(state_file).await?;
        let state: Value = serde_json::from_str(&content)?;

        if let Some(last_run) = state.get(path).and_then(|v| v.as_u64()) {
            Ok(Some(UNIX_EPOCH + std::time::Duration::from_secs(last_run)))
        } else {
            Ok(None)
        }
    }

    // Add this function to save cron execution time
    pub async fn save_cron_execution(pipe_dir: &Path, path: &str) -> Result<()> {
        let state_file = pipe_dir.join(".cron_state.json");

        let mut state: Value = if state_file.exists() {
            let content = tokio::fs::read_to_string(&state_file).await?;
            serde_json::from_str(&content)?
        } else {
            json!({})
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if let Some(obj) = state.as_object_mut() {
            obj.insert(path.to_string(), json!(now));
        }

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(state_file)
            .await?;

        file.write_all(serde_json::to_string_pretty(&state)?.as_bytes())
            .await?;
        Ok(())
    }

    // Update the run_cron_schedule function
    async fn run_cron_schedule(
        pipe: &str,
        base_url: &str,
        path: &str,
        secret: &str,
        schedule: &str,
    ) {
        let schedule = match cron::Schedule::from_str(schedule) {
            Ok(s) => s,
            Err(e) => {
                error!("invalid cron schedule: {}", e);
                return;
            }
        };

        let client = reqwest::Client::new();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", secret)).unwrap(),
        );

        // Get pipe directory for state persistence
        let screenpipe_dir = std::env::var("SCREENPIPE_DIR").expect("SCREENPIPE_DIR not set");
        let pipe_dir = PathBuf::from(screenpipe_dir).join("pipes").join(pipe);

        // Get last execution time
        let last_run = match get_last_cron_execution(&pipe_dir, path).await {
            Ok(time) => time,
            Err(e) => {
                error!("failed to get last cron execution: {}", e);
                None
            }
        };

        loop {
            let now = chrono::Utc::now();
            let next = if let Some(last) = last_run {
                // Get next occurrence after the last execution
                let last_chrono = chrono::DateTime::<chrono::Utc>::from(last);
                schedule.after(&last_chrono).next()
            } else {
                schedule.after(&now).next()
            };

            let next = match next {
                Some(next) => next,
                None => continue,
            };

            let duration = (next - now)
                .to_std()
                .unwrap_or(tokio::time::Duration::from_secs(1));
            tokio::time::sleep(duration).await;

            debug!("executing cron job for pipe {} at path {}", pipe, path);

            match client
                .get(&format!("{}{}", base_url, path))
                .headers(headers.clone())
                .send()
                .await
            {
                Ok(res) => {
                    if res.status().is_success() {
                        // Save successful execution time
                        if let Err(e) = save_cron_execution(&pipe_dir, path).await {
                            error!("failed to save cron execution time: {}", e);
                        }
                    } else {
                        error!("cron job failed with status: {}", res.status());
                        if let Ok(text) = res.text().await {
                            error!("error response: {}", text);
                        }
                    }
                }
                Err(e) => error!("failed to execute cron job: {}", e),
            }
        }
    }
}

#[cfg(feature = "pipes")]
pub use pipes::*;
