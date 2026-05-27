use anyhow::Result;
use cascade_engine::db::StateDb;
use cascade_engine::vfs::VfsTree;
use cascade_presenter_nfs::{NfsPresenter, VfsPresenter};
use std::path::PathBuf;
use std::sync::Arc;

/// Start the Cascade daemon.
pub async fn start(mount_point: Option<&str>) -> Result<()> {
    tracing::info!("Starting Cascade daemon");

    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".cascade"))
        .join("cascade");
    let db_path = config_dir.join("state.db");

    let _db = StateDb::open(&db_path)?;
    tracing::info!("State database opened at {:?}", db_path);

    let backend = cascade_backend_gdrive::create_backend(&toml::Value::Table(Default::default()))?;
    let root = Arc::from(backend);

    let _tree = VfsTree::new(root);
    tracing::info!("VFS tree initialised");

    let mount_path = mount_point
        .map(|m| {
            let p = PathBuf::from(m);
            if p.starts_with("~") {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(p.strip_prefix("~").unwrap_or(&p))
            } else {
                p
            }
        })
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("Cloud")
        });

    let presenter = NfsPresenter::new(&mount_path);
    presenter.start(&mount_path).await?;

    tracing::info!("Cascade mounted at {:?}", mount_path);
    println!("Cascade started. Mount point: {:?}", mount_path);

    Ok(())
}

/// Stop the Cascade daemon.
pub async fn stop() -> Result<()> {
    tracing::info!("Stopping Cascade daemon");

    let presenter = NfsPresenter::default_mount();
    presenter.stop().await?;

    println!("Cascade stopped.");
    Ok(())
}
