use super::common::copy_to;
use clap::Subcommand;
use curvine_core_error::CommonResult;
use curvine_fs_api::{CurvineURI, FileSystem, Reader};
use curvine_unified_fs::UnifiedFileSystem;
use std::path::{Path, PathBuf};
use tokio::fs;

#[derive(Subcommand, Debug)]
pub enum GetCommand {
    /// Get file to local
    Get {
        #[clap(help = "Path of the file to get")]
        path: String,
        #[clap(help = "Local destination path")]
        local_path: PathBuf,
    },
}

/// Sibling path a download is written to before it is published.
///
/// The suffix keeps the temporary file in the destination directory, so the
/// final `rename` stays within one filesystem and cannot fail with `EXDEV`.
fn part_path(final_path: &Path) -> PathBuf {
    let mut name = final_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".curvine-part");
    final_path.with_file_name(name)
}

impl GetCommand {
    pub async fn execute(&self, client: UnifiedFileSystem) -> CommonResult<()> {
        match self {
            GetCommand::Get { path, local_path } => {
                let path = CurvineURI::new(path)?;

                // Check if the path exists and is a file
                match client.get_status(&path).await {
                    Ok(status) => {
                        if status.is_dir {
                            return Err(format!(
                                "Cannot download '{}': Is a directory",
                                path.full_path()
                            )
                            .into());
                        }

                        // Determine the final local path
                        let mut final_local_path = if local_path.exists() && local_path.is_dir()
                            || local_path
                                .to_string_lossy()
                                .ends_with(std::path::MAIN_SEPARATOR)
                        {
                            // If local_path is a directory or ends with a path separator, append the filename from the remote path
                            let filename = path.name();
                            local_path.join(filename)
                        } else if !local_path
                            .to_string_lossy()
                            .contains(std::path::MAIN_SEPARATOR)
                        {
                            // If local_path is just a filename without path, use current directory
                            let filename = local_path.to_string_lossy().to_string();
                            std::env::current_dir()
                                .unwrap_or_else(|_| PathBuf::from("."))
                                .join(filename)
                        } else {
                            local_path.clone()
                        };

                        // Check if parent directory exists, refuse to execute if it doesn't
                        if let Some(parent) = final_local_path.parent() {
                            if !parent.exists() {
                                return Err(format!(
                                    "Parent directory '{}' does not exist. Please create it first.",
                                    parent.display()
                                )
                                .into());
                            }
                        }

                        // If the final_local_path itself is a directory (ends with separator), create it
                        if final_local_path
                            .to_string_lossy()
                            .ends_with(std::path::MAIN_SEPARATOR)
                        {
                            if let Err(e) = fs::create_dir_all(&final_local_path).await {
                                return Err(format!(
                                    "Cannot create directory '{}': {}",
                                    final_local_path.display(),
                                    e
                                )
                                .into());
                            }
                            // Append the filename to the directory path
                            let filename = path.name();
                            final_local_path = final_local_path.join(filename);
                        }

                        // Write to a sibling temp file and publish it with a
                        // rename once the transfer succeeded: truncating the
                        // destination first destroyed a pre-existing local file
                        // even when the download failed (#2315).
                        let part_path = part_path(&final_local_path);
                        let mut file = match fs::File::create(&part_path).await {
                            Ok(file) => file,
                            Err(e) => {
                                return Err(format!(
                                    "Cannot create local file '{}': {}",
                                    part_path.display(),
                                    e
                                )
                                .into());
                            }
                        };

                        println!("Downloading to {}", final_local_path.display());

                        // Open the remote file and read its content
                        match client.open(&path).await {
                            Ok(mut reader) => {
                                let chunk_size = client.conf().client.read_chunk_size;
                                let copied = copy_to(&mut reader, &mut file, chunk_size).await;
                                let bytes_read = match copied {
                                    Ok(bytes_read) => bytes_read,
                                    Err(e) => {
                                        // Only the partial download is ours to
                                        // remove; the destination was never
                                        // touched.
                                        drop(file);
                                        let _ = fs::remove_file(&part_path).await;
                                        return Err(format!(
                                            "Cannot download '{}': {}",
                                            path.full_path(),
                                            e
                                        )
                                        .into());
                                    }
                                };

                                if let Err(e) = reader.complete().await {
                                    drop(file);
                                    let _ = fs::remove_file(&part_path).await;
                                    return Err(format!(
                                        "Cannot complete read of '{}': {}",
                                        path.full_path(),
                                        e
                                    )
                                    .into());
                                }

                                drop(file);
                                if let Err(e) = fs::rename(&part_path, &final_local_path).await {
                                    let _ = fs::remove_file(&part_path).await;
                                    return Err(format!(
                                        "Cannot write local file '{}': {}",
                                        final_local_path.display(),
                                        e
                                    )
                                    .into());
                                }

                                println!("Successfully downloaded {} bytes", bytes_read);
                                Ok(())
                            }
                            Err(e) => {
                                drop(file);
                                let _ = fs::remove_file(&part_path).await;
                                Err(format!(
                                    "Cannot open remote file '{}': {}",
                                    path.full_path(),
                                    e
                                )
                                .into())
                            }
                        }
                    }
                    Err(e) => Err(format!("Cannot access '{}': {}", path.full_path(), e).into()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::part_path;
    use std::path::Path;

    #[test]
    fn the_partial_download_is_a_sibling_of_the_destination() {
        let part = part_path(Path::new("/data/out/file.bin"));
        assert_eq!(part, Path::new("/data/out/file.bin.curvine-part"));
        // Same directory, so the publishing rename cannot cross a filesystem.
        assert_eq!(part.parent(), Some(Path::new("/data/out")));
        assert_ne!(part, Path::new("/data/out/file.bin"));
    }
}
