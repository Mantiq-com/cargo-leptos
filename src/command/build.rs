use std::sync::Arc;

use tokio::try_join;

use crate::ext::compress;
use crate::internal_prelude::*;
use crate::{
    compile,
    compile::ChangeSet,
    config::{Config, Project},
    ext::fs,
};

pub async fn build_all(conf: &Config) -> Result<()> {
    let mut first_failed_project = None;

    for proj in &conf.projects {
        debug!("Building project: {}, {}", proj.name, proj.working_dir);
        if !build_proj(proj).await? && first_failed_project.is_none() {
            first_failed_project = Some(proj);
        }
    }

    if let Some(proj) = first_failed_project {
        Err(eyre!("Failed to build {}", proj.name))
    } else {
        Ok(())
    }
}

async fn build_frontend(proj: &Arc<Project>, changes: &ChangeSet) -> Result<bool> {
    let front_hdl = compile::front(proj, changes).await;
    let assets_hdl = compile::assets(proj, changes).await;
    let style_hdl = compile::style(proj, changes).await;

    let (front, assets, style) = try_join!(front_hdl, assets_hdl, style_hdl)?;

    if !front?.is_success() || !assets?.is_success() || !style?.is_success() {
        return Ok(false);
    }

    if proj.hash_files {
        compile::add_hashes_to_site(proj)?;
    }

    // it is important to do the precompression of the static files before building the
    // server to make it possible to include them as assets into the binary itself
    if proj.release && proj.precompress {
        compress::compress_static_files(proj.site.root_dir.clone().into()).await?;
    }

    Ok(true)
}

/// Whether a previous process recorded the wasm hash its package output was
/// generated from, so that output is worth keeping through the startup wipe.
fn front_output_may_be_reused(proj: &Project) -> bool {
    compile::persists_front_wasm_hash(proj) && compile::front_wasm_hash_file(proj).exists()
}

/// Build the project. Returns true if the build was successful
pub async fn build_proj(proj: &Arc<Project>) -> Result<bool> {
    if !proj.site.root_dir.exists() {
        fs::create_dir_all(&proj.site.root_dir).await.dot()?;
    } else if front_output_may_be_reused(proj) {
        // The previous process left a hash of the wasm its package output was
        // generated from. Keep that output: if the wasm links to the same bytes
        // again, the front step reuses it instead of splitting and running
        // wasm-bindgen; otherwise the front step removes it before generating.
        let pkg_dir = proj.site.root_relative_pkg_dir();
        fs::rm_dir_content_except(&proj.site.root_dir, &pkg_dir)
            .await
            .dot()?;
    } else {
        fs::rm_dir_content(&proj.site.root_dir).await.dot()?;
    }

    let changes = ChangeSet::all_changes();
    let needs_frontend = !proj.build_server_only;
    let needs_server = !proj.build_frontend_only;
    let can_parallelize = !(proj.hash_files || proj.release && proj.precompress);

    if can_parallelize && needs_frontend && needs_server {
        let front_hdl = compile::front(proj, &changes).await;
        let assets_hdl = compile::assets(proj, &changes).await;
        let style_hdl = compile::style(proj, &changes).await;
        let server_hdl = compile::server(proj, &changes).await;

        let (front, assets, style, server) =
            try_join!(front_hdl, assets_hdl, style_hdl, server_hdl)?;

        if !front?.is_success()
            || !assets?.is_success()
            || !style?.is_success()
            || !server?.is_success()
        {
            return Ok(false);
        }
    } else {
        if needs_frontend && !build_frontend(proj, &changes).await? {
            return Ok(false);
        }
        if needs_server && !compile::server(proj, &changes).await.await??.is_success() {
            return Ok(false);
        }
    }

    Ok(true)
}
