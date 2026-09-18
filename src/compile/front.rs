use super::{Change, ChangeSet};
use crate::{
    config::Project,
    ext::{
        eyre::AnyhowCompatWrapErr,
        fs,
        sync::{wait_interruptible, wait_piped_interruptible, CommandResult, OutputExt},
        Exe, PathBufExt,
    },
    internal_prelude::*,
    logger::GRAY,
    signal::{Interrupt, Outcome, Product},
    wasm_split_tools,
};
use camino::{Utf8Path, Utf8PathBuf};
use futures::{stream, StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use swc::{
    config::{IsModule, JsMinifyOptions},
    try_with_handler, BoolOrDataConfig, JsMinifyExtras,
};
use swc_common::{FileName, SourceMap, GLOBALS};
use tokio::{
    process::{Child, Command},
    task::JoinHandle,
};

pub async fn front(
    proj: &Arc<Project>,
    changes: &ChangeSet,
) -> JoinHandle<Result<Outcome<Product>>> {
    let proj = proj.clone();
    let changes = changes.clone();
    tokio::spawn(async move {
        if !changes.need_front_build() {
            trace!("Front no changes to rebuild");
            return Ok(Outcome::Success(Product::None));
        }

        let pkg_dir = proj.site.root_relative_pkg_dir();

        let mut files = vec![proj.lib.wasm_file.dest.clone()];

        fs::create_dir_all(&pkg_dir).await?;

        let (envs, line, process) = front_cargo_process("build", true, &proj)?;

        debug!("Running {}", GRAY.paint(&line));
        match wait_interruptible("Cargo", process, Interrupt::subscribe_any()).await? {
            CommandResult::Interrupted => return Ok(Outcome::Stopped),
            CommandResult::Failure(_) => return Ok(Outcome::Failed),
            _ => {}
        }
        debug!("Cargo envs: {}", GRAY.paint(envs));
        info!("Cargo finished {}", GRAY.paint(line));

        let previous_wasm_hash = take_front_wasm_hash(&proj);

        let input_wasm = tokio::fs::read(&proj.lib.wasm_file.source).await?;
        let wasm_hash = seahash::hash(&input_wasm);
        if front_wasm_unchanged(&proj, previous_wasm_hash, wasm_hash) {
            // Re-insert the entry taken above: the output it describes is
            // untouched.
            record_front_wasm(&proj, wasm_hash);
            info!(
                "Finished generating JS/WASM for front (wasm unchanged; reusing previous output)"
            );
            // A watched additional file can influence the running app without
            // changing the wasm (that is why it is watched): keep the browser
            // reload the full pipeline used to cause for those changes.
            let product = if changes.contains(&Change::Additional) {
                Product::Assets
            } else {
                Product::None
            };
            return Ok(Outcome::Success(product));
        }

        // The previous output is about to be replaced; drop it first. Split
        // chunks are numbered and the lazy-module files are named after
        // their module, so a rebuild that produces fewer chunks, or drops a
        // lazy module, would otherwise leave the old files in the package
        // directory.
        remove_front_outputs(&pkg_dir, &proj.lib.output_name).await?;

        if proj.split {
            info!("Front splitting out lazy-loaded WASM files");
            let start_time = tokio::time::Instant::now();

            let split_files = wasm_split_tools::wasm_split(&input_wasm, false, &proj).await?;
            files.extend(split_files);

            let end_time = tokio::time::Instant::now();

            info!("Finished WASM splitting in {:?}", end_time - start_time);
        }

        // The module can be gigabytes; release it before wasm-bindgen loads
        // its own copy.
        drop(input_wasm);

        let outcome = bindgen(proj.clone(), files).await.dot();
        if let Ok(Outcome::Success(_)) = &outcome {
            record_front_wasm(&proj, wasm_hash);
        }
        outcome
    })
}

fn front_wasm_registry() -> &'static Mutex<HashMap<String, u64>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Removes and returns the hash of the wasm consumed by the last successful
/// split/bindgen run. Taken rather than read so the entry only exists while
/// the output it describes is intact: the caller re-inserts it once this run
/// ends with the output known good, and a failed or interrupted run leaves it
/// absent.
///
/// The in-process registry answers within a `watch` session. A new process
/// falls back to the hash the previous process persisted beside the linked
/// wasm, so a launch whose sources are unchanged can reuse the output that
/// [`crate::command::build::build_proj`] kept in place for it.
fn take_front_wasm_hash(proj: &Project) -> Option<u64> {
    let in_process = front_wasm_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(proj.lib.wasm_file.source.as_str());
    if !persists_front_wasm_hash(proj) {
        return in_process;
    }
    // Taken as well, even when the registry answered: while this run's
    // pipeline is in flight no record may vouch for the output on disk.
    let persisted = take_persisted_front_wasm_hash(&front_wasm_hash_file(proj));
    in_process.or(persisted)
}

/// Reports whether the front wasm artifact (hashed into `hash`) is
/// byte-identical to the input of the last successful split/bindgen run
/// (`previous_hash`, taken by the caller from the registry or the persisted
/// file).
///
/// A rebuild caused by a change that only affects the server binary (or a
/// watched non-Rust file) does not alter the wasm: splitting and wasm-bindgen
/// would reproduce identical output, so the caller can skip them and keep the
/// previous output. A content hash rather than mtime, because a relink from
/// unchanged inputs rewrites the file without changing its bytes. A missing
/// bindgen output always reports "changed", which is also what the first
/// build of a process sees when no hash was persisted and the site directory
/// was wiped at startup.
fn front_wasm_unchanged(proj: &Project, previous_hash: Option<u64>, hash: u64) -> bool {
    previous_hash == Some(hash) && proj.lib.js_file.dest.exists()
}

/// Records the hash of the wasm the current site output was generated from,
/// so the next build can skip the pipeline when the wasm is unchanged.
/// Reached only with that output known good -- after a successful pipeline
/// run, or on a skip that left it untouched; after a failed or interrupted
/// run the entry stays absent and the pipeline runs again.
fn record_front_wasm(proj: &Project, hash: u64) {
    front_wasm_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(proj.lib.wasm_file.source.to_string(), hash);
    if persists_front_wasm_hash(proj) {
        let file = front_wasm_hash_file(proj);
        if let Err(err) = std::fs::write(&file, hash.to_string()) {
            warn!("Could not persist the front wasm hash to {file}: {err}");
        }
    }
}

/// The persisted hash serves development watches. A release build always
/// regenerates (its output is what ships), and hashed file names rename the
/// outputs after generation, so their presence cannot be read back from the
/// unhashed paths the gate checks.
pub fn persists_front_wasm_hash(proj: &Project) -> bool {
    !proj.release && !proj.hash_files
}

/// Beside the linked wasm in the target directory, which survives a restart;
/// the site directory does not, and is the thing the hash vouches for.
pub fn front_wasm_hash_file(proj: &Project) -> Utf8PathBuf {
    Utf8PathBuf::from(format!("{}.front-hash", proj.lib.wasm_file.source))
}

/// Reads and removes the persisted hash, mirroring the registry's take
/// semantics: while the pipeline runs, no record vouches for the output.
fn take_persisted_front_wasm_hash(file: &Utf8Path) -> Option<u64> {
    let contents = std::fs::read_to_string(file).ok()?;
    if let Err(err) = std::fs::remove_file(file) {
        warn!("Could not remove the front wasm hash file {file}: {err}");
    }
    let hash = contents.trim().parse().ok();
    if hash.is_none() {
        warn!("Ignoring the unreadable front wasm hash file {file}");
    }
    hash
}

/// Removes what the split and bindgen steps write into the package
/// directory: the main module, its JS glue and type declarations, the snippet
/// directory, and the split chunks, loader and manifest. Everything else in
/// the directory -- the stylesheet the style step writes concurrently, and
/// whatever the user placed there -- is left alone.
async fn remove_front_outputs(pkg_dir: &Utf8Path, output_name: &str) -> Result<()> {
    if !pkg_dir.exists() {
        return Ok(());
    }
    let owned_files = [
        format!("{output_name}.js"),
        format!("{output_name}.wasm"),
        format!("{output_name}_bg.wasm"),
        format!("{output_name}.d.ts"),
        format!("{output_name}_bg.wasm.d.ts"),
    ];
    let mut entries = fs::read_dir(pkg_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_dir = entry.file_type().await?.is_dir();
        if is_dir {
            if name == "snippets" {
                fs::remove_dir_all(entry.path()).await?;
            }
            continue;
        }
        let split_output = name.starts_with("__wasm_split")
            || name.starts_with("split_load_")
            || name.starts_with("chunk_");
        if split_output || owned_files.iter().any(|owned| *owned == name) {
            fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

pub fn front_cargo_process(
    cmd: &str,
    wasm: bool,
    proj: &Project,
) -> Result<(String, String, Child)> {
    front_cargo_process_with_args(cmd, wasm, proj, None)
}

pub fn front_cargo_process_with_args(
    cmd: &str,
    wasm: bool,
    proj: &Project,
    additional_args: Option<&[String]>,
) -> Result<(String, String, Child)> {
    let mut command = Command::new("cargo");
    let (envs, line) = build_cargo_front_cmd(cmd, wasm, proj, &mut command, additional_args);
    Ok((envs, line, command.spawn()?))
}

pub fn build_cargo_front_cmd(
    cmd: &str,
    wasm: bool,
    proj: &Project,
    command: &mut Command,
    additional_args: Option<&[String]>,
) -> (String, String) {
    let mut args = vec![
        cmd.to_string(),
        format!("--package={}", proj.lib.name.as_str()),
        "--lib".to_string(),
        format!("--target-dir={}", &proj.lib.front_target_path),
    ];

    if wasm {
        args.push("--target=wasm32-unknown-unknown".to_string());
    }

    if !proj.lib.default_features {
        args.push("--no-default-features".to_string());
    }

    if !proj.lib.features.is_empty() {
        args.push(format!("--features={}", proj.lib.features.join(",")));
    }

    // Add cargo flags to cargo command
    args.extend_from_slice(&proj.lib.cargo_args);

    proj.lib.profile.add_to_args(&mut args);

    if let Some(add_args) = additional_args {
        args.extend_from_slice(add_args);
    }

    let envs = proj.to_envs(wasm);

    let envs_str = envs
        .iter()
        .map(|(name, val)| format!("{name}={val}"))
        .collect::<Vec<_>>()
        .join(" ");

    command.args(&args).envs(envs);

    let line = super::build_cargo_command_string(command);
    trace!(?envs_str, ?line, "Constructed cargo build front cmd");
    (envs_str, line)
}

async fn bindgen(proj: Arc<Project>, all_wasm_files: Vec<Utf8PathBuf>) -> Result<Outcome<Product>> {
    let wasm_file = &proj.lib.wasm_file;

    info!("Front generating JS/WASM with wasm-bindgen");

    let wasm_file_input = if proj.split {
        let mut source = proj.lib.wasm_file.source.clone();
        source.set_file_name(format!("{}_split.wasm", source.file_stem().unwrap()));
        source
    } else {
        proj.lib.wasm_file.source.clone()
    };

    let start_time = tokio::time::Instant::now();
    /* // see:
    // https://github.com/rustwasm/wasm-bindgen/blob/main/crates/cli-support/src/lib.rs#L95
    // https://github.com/rustwasm/wasm-bindgen/blob/main/crates/cli/src/bin/wasm-bindgen.rs#L13
    let mut bindgen = Bindgen::new()
        .keep_lld_exports(proj.split)
        .demangle(!proj.split)
        .debug(proj.wasm_debug)
        .keep_debug(proj.wasm_debug)
        .input_path(&wasm_file_input)
        .out_name(&proj.lib.output_name)
        .web(true)
        .dot_anyhow()?
        .generate_output()
        .dot_anyhow()?; */

    let wasm_bindgen = Exe::WasmBindgen {
        project_root: &proj.working_dir,
    }
    .get()
    .await
    .dot()?;

    let args = [
        Some("--target=web".to_string()),
        proj.split.then(|| "--keep-lld-exports".into()),
        proj.split.then(|| "--no-demangle".into()),
        proj.wasm_debug.then(|| "--debug".into()),
        proj.wasm_debug.then(|| "--keep-debug".into()),
        Some(format!("--out-name={}", proj.lib.output_name)),
        Some(format!(
            "--out-dir={}",
            wasm_file.dest.clone().without_last()
        )),
        Some(wasm_file_input.into()),
    ]
    .into_iter()
    .flatten();

    let mut cmd = Command::new(wasm_bindgen);
    cmd.args(args.clone());

    match wait_piped_interruptible(
        "wasm-bindgen",
        cmd,
        crate::signal::Interrupt::subscribe_any(),
    )
    .await?
    {
        CommandResult::Interrupted => Ok(Outcome::Stopped),
        CommandResult::Failure(output) => {
            error!("wasm-bindgen failed with:");
            println!("{}", output.stderr());
            bail!("wasm-bindgen failed")
        }
        CommandResult::Success(_) => {
            let bindgen_emit_end_time = tokio::time::Instant::now();
            debug!(
                "Finished emitting wasm-bindgen in {:?}",
                bindgen_emit_end_time - start_time
            );

            // rename emitted wasm output file name from {output_name}_bg.wasm to {output_name}.wasm for
            // backward compatibility with leptos' `HydrationScripts`
            fs::rename(
                wasm_file
                    .dest
                    .clone()
                    .without_last()
                    .join(format!("{}_bg.wasm", &proj.lib.output_name)),
                &wasm_file.dest,
            )
            .await
            .dot()?;

            if proj.release {
                let parallelism = std::thread::available_parallelism()
                    .map(std::num::NonZero::get)
                    .unwrap_or(1);

                let wasm_opt = Exe::WasmOpt.get().await.dot()?;

                stream::iter(all_wasm_files)
                    .map(|file| optimize(&proj, file, &wasm_opt))
                    .buffer_unordered(parallelism)
                    .try_collect::<()>()
                    .await?;
            }

            let wasm_optimize_end_time = tokio::time::Instant::now();
            debug!(
                "Finished optimizing WASM in {:?}",
                wasm_optimize_end_time - bindgen_emit_end_time
            );

            if proj.js_minify {
                let js_file_name = wasm_file
                    .dest
                    .clone()
                    .without_last()
                    .join(format!("{}.js", &proj.lib.output_name));
                let js = fs::read_to_string(&js_file_name).await?;
                proj.site
                    .updated_with(&proj.lib.js_file, minify(&js)?.as_bytes())
                    .await
                    .dot()?;

                let js_minify_end_time = tokio::time::Instant::now();
                debug!(
                    "Finished minifying JS in {:?}",
                    js_minify_end_time - wasm_optimize_end_time
                );
            };

            let front_end_time = tokio::time::Instant::now();
            info!(
                "Finished generating JS/WASM for front in {:?}",
                front_end_time - start_time
            );

            Ok(Outcome::Success(Product::Front))
        }
    }
}

async fn optimize(proj: &Project, file: Utf8PathBuf, wasm_opt: &Path) -> Result<()> {
    let mut args: Vec<&str> = if let Some(features) = &proj.wasm_opt_features {
        features.iter().map(|f| f.as_str()).collect()
    } else {
        vec![
            "-Oz",
            "--enable-bulk-memory",
            "--enable-nontrapping-float-to-int",
        ]
    };
    args.extend_from_slice(&[file.as_str(), "-o", file.as_str()]);

    let mut cmd = Command::new(wasm_opt);
    cmd.args(args.clone());

    trace!("WASM running wasm-opt {}", args.join(" "));

    match wait_piped_interruptible("wasm-opt", cmd, crate::signal::Interrupt::subscribe_any())
        .await?
    {
        CommandResult::Success(_) => Ok(()),
        CommandResult::Interrupted => bail!("wasm-opt was interrupted"),
        CommandResult::Failure(output) => {
            error!("wasm-opt failed with:");
            println!("{}", output.stderr());
            bail!("wasm-opt optimization failed")
        }
    }
}

fn minify<JS: AsRef<str>>(js: JS) -> Result<String> {
    let cm = Arc::<SourceMap>::default();

    let c = swc::Compiler::new(cm.clone());
    let output = GLOBALS
        .set(&Default::default(), || {
            try_with_handler(cm.clone(), Default::default(), |handler| {
                let fm = cm.new_source_file(Arc::new(FileName::Anon), js.as_ref().to_string());

                use anyhow::Context;

                c.minify(
                    fm,
                    handler,
                    &JsMinifyOptions {
                        compress: BoolOrDataConfig::from_bool(true),
                        mangle: BoolOrDataConfig::from_bool(true),
                        // keep_classnames: true,
                        // keep_fnames: true,
                        module: IsModule::Bool(true),
                        ..Default::default()
                    },
                    JsMinifyExtras::default(),
                )
                .context("failed to minify")
            })
        })
        .map_err(|e| e.to_pretty_error())
        .wrap_anyhow_err("Failed to minify")?;

    Ok(output.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use temp_dir::TempDir;

    fn utf8(dir: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()
    }

    #[test]
    fn a_persisted_hash_is_read_once_and_then_gone() {
        let dir = TempDir::new().unwrap();
        let file = utf8(&dir).join("app.wasm.front-hash");
        fs::write(&file, "1234567890123\n").unwrap();

        assert_eq!(take_persisted_front_wasm_hash(&file), Some(1234567890123));
        assert!(!file.exists(), "the take removes the file");
        assert_eq!(take_persisted_front_wasm_hash(&file), None);
    }

    #[test]
    fn an_unreadable_persisted_hash_is_ignored_and_removed() {
        let dir = TempDir::new().unwrap();
        let file = utf8(&dir).join("app.wasm.front-hash");
        fs::write(&file, "not a number").unwrap();

        assert_eq!(take_persisted_front_wasm_hash(&file), None);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn removing_front_outputs_leaves_the_stylesheet_and_foreign_files() {
        let dir = TempDir::new().unwrap();
        let pkg_dir = utf8(&dir);
        for name in [
            "app.js",
            "app.wasm",
            "app_bg.wasm",
            "app.d.ts",
            "app_bg.wasm.d.ts",
            "__wasm_split.______________________.js",
            "__wasm_split_manifest.json",
            "split_load_settings_123.wasm",
            "chunk_7.wasm",
            "app.css",
            "other.js",
            "notes.txt",
        ] {
            fs::write(pkg_dir.join(name), b"x").unwrap();
        }
        fs::create_dir_all(pkg_dir.join("snippets").join("inline0")).unwrap();
        fs::write(pkg_dir.join("snippets/inline0/inline0.js"), b"x").unwrap();
        fs::create_dir_all(pkg_dir.join("fonts")).unwrap();
        fs::write(pkg_dir.join("fonts/a.woff2"), b"x").unwrap();

        remove_front_outputs(&pkg_dir, "app").await.unwrap();

        let mut remaining: Vec<String> = fs::read_dir(&pkg_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();
        assert_eq!(remaining, ["app.css", "fonts", "notes.txt", "other.js"]);
    }

    #[tokio::test]
    async fn removing_front_outputs_from_a_missing_directory_is_fine() {
        let dir = TempDir::new().unwrap();
        let pkg_dir = utf8(&dir).join("never-built");

        remove_front_outputs(&pkg_dir, "app").await.unwrap();

        assert!(!pkg_dir.exists());
    }
}
