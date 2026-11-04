//! This module began as a port of the wasm-split-prototype first developed
//! by @jbms at https://github.com/jbms/wasm-split-prototype
//! under the Apache License: https://github.com/jbms/wasm-split-prototype/blob/main/LICENSE

use crate::{config::Project, internal_prelude::*};
use camino::Utf8PathBuf;

pub async fn wasm_split(
    input_wasm: &[u8],
    verbose: bool,
    proj: &Project,
) -> Result<Vec<Utf8PathBuf>> {
    let dest_file = &proj.lib.wasm_file.dest;
    let dest_dir = dest_file.parent().expect("no destination directory");
    let source_file = &proj.lib.wasm_file.source;
    // split chunks are written relative to main JS file
    let main_module = &format!("./{}.js", proj.lib.output_name);

    let mut main_out_file = source_file.clone();
    main_out_file.set_file_name(format!("{}_split.wasm", source_file.file_stem().unwrap()));

    // The cost of one more chunk request, in downloaded bytes, for merging shared chunks.
    // Unset keeps the splitter's default for the target; 0 keeps every chunk.
    let cost_model = match std::env::var("WASM_SPLIT_REQUEST_COST") {
        Ok(value) => match value
            .parse::<usize>()
            .wrap_err_with(|| format!("WASM_SPLIT_REQUEST_COST={value:?} is not a byte count"))?
        {
            0 => Some(wasm_split_cli_support::CostModel::None),
            request_cost => Some(wasm_split_cli_support::CostModel::Expected(
                wasm_split_cli_support::BernoulliCost {
                    request_cost,
                    load_probability: 0.05,
                },
            )),
        },
        Err(std::env::VarError::NotPresent) => None,
        Err(err) => return Err(err).wrap_err("Could not read WASM_SPLIT_REQUEST_COST"),
    };

    let split_wasm = wasm_split_cli_support::transform({
        let mut opts = wasm_split_cli_support::Options::new(input_wasm);
        opts.output_dir = dest_dir.as_std_path();
        opts.main_out_path = main_out_file.as_std_path();
        opts.main_module = main_module;
        opts.link_name = "./__wasm_split.______________________.js";
        opts.verbose = verbose;
        opts.cost_model = cost_model;
        opts
    })?;
    tokio::fs::write(
        dest_dir.join("__wasm_split_manifest.json"),
        serde_json::to_string_pretty(&split_wasm.prefetch_map)
            .expect("could not serialize manifest file"),
    )
    .await?;

    Ok(split_wasm
        .split_modules
        .into_iter()
        .map(|path| path.try_into().unwrap())
        .collect())
}
