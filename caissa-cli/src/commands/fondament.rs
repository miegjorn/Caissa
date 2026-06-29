/// Fondament registry operations — publish definition YAMLs to MinIO/S3.
///
/// Storage layout in the `fondament-registry` bucket:
///   {namespace}/{name}/{version}.yaml  — immutable definition
///   {namespace}/{name}/latest          — plain-text latest version string

use aws_credential_types::Credentials;
use aws_sdk_s3::{config::Region, primitives::ByteStream};
use std::path::Path;

#[derive(serde::Deserialize)]
struct DefHeader {
    id: String,
    version: Option<String>,
}

pub async fn publish(
    file: Option<&str>,
    all: bool,
    definitions_dir: &str,
    registry_url: &str,
    bucket: &str,
    force: bool,
) -> anyhow::Result<()> {
    // ── Build S3 client with custom endpoint for MinIO ────────────────────
    let access_key = std::env::var("AWS_ACCESS_KEY_ID")
        .unwrap_or_else(|_| "occitan".into());
    let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
        .unwrap_or_else(|_| "occitan-dev".into());

    let creds = Credentials::new(access_key, secret_key, None, None, "static");

    let config = aws_sdk_s3::Config::builder()
        .endpoint_url(registry_url)
        .region(Region::new("us-east-1"))
        .credentials_provider(creds)
        .force_path_style(true)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .build();

    let client = aws_sdk_s3::Client::from_conf(config);

    // ── Collect files to publish ─────────────────────────────────────────
    let files: Vec<std::path::PathBuf> = if let Some(path) = file {
        vec![Path::new(path).to_path_buf()]
    } else if all {
        collect_yaml_files(definitions_dir)?
    } else {
        anyhow::bail!("Specify --file <path> or --all");
    };

    if files.is_empty() {
        eprintln!("[fondament publish] no YAML files found in {definitions_dir}");
        return Ok(());
    }

    let mut published = 0u32;
    let mut skipped = 0u32;

    for path in &files {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;

        // Parse just id + version
        let header: DefHeader = serde_yaml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;

        let version = header.version.as_deref().unwrap_or("1.0.0");

        // Split id into namespace/name
        let (namespace, name) = split_id(&header.id, path)?;

        let def_key = format!("{namespace}/{name}/{version}.yaml");
        let latest_key = format!("{namespace}/{name}/latest");

        // Check if version already exists
        if !force {
            let exists = client
                .head_object()
                .bucket(bucket)
                .key(&def_key)
                .send()
                .await
                .is_ok();

            if exists {
                eprintln!(
                    "[fondament publish] skipped {}@{version} (already exists, use --force to overwrite)",
                    header.id
                );
                skipped += 1;
                continue;
            }
        }

        // PUT the definition YAML
        client
            .put_object()
            .bucket(bucket)
            .key(&def_key)
            .content_type("application/yaml")
            .body(ByteStream::from(content.into_bytes()))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("failed to PUT {def_key}: {e}"))?;

        // PUT the latest pointer
        client
            .put_object()
            .bucket(bucket)
            .key(&latest_key)
            .content_type("text/plain")
            .body(ByteStream::from(version.as_bytes().to_vec()))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("failed to PUT {latest_key}: {e}"))?;

        println!("[fondament publish] published {}@{version}", header.id);
        published += 1;
    }

    println!(
        "[fondament publish] done — published {published}, skipped {skipped} (already exist)"
    );
    Ok(())
}

fn split_id<'a>(id: &'a str, path: &Path) -> anyhow::Result<(&'a str, &'a str)> {
    match id.split_once('/') {
        Some((ns, name)) => Ok((ns, name)),
        None => anyhow::bail!(
            "invalid id '{}' in {}: expected namespace/name format",
            id,
            path.display()
        ),
    }
}

fn collect_yaml_files(dir: &str) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut results = Vec::new();
    collect_yaml_recursive(Path::new(dir), &mut results)?;
    Ok(results)
}

fn collect_yaml_recursive(
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
) -> anyhow::Result<()> {
    if !dir.exists() {
        anyhow::bail!("definitions directory '{}' does not exist", dir.display());
    }
    for entry in std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read dir {}: {e}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_yaml_recursive(&path, out)?;
        } else if let Some(ext) = path.extension() {
            if ext == "yaml" || ext == "yml" {
                out.push(path);
            }
        }
    }
    Ok(())
}
