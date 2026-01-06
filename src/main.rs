use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;
use google_cloud_auth::credentials::CredentialsFile;
use google_cloud_storage::client::{Client, ClientConfig};
use google_cloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::write::FileOptions;
use zip::ZipWriter;


const KEEP_LOCAL_BACKUP_DAYS: i64 = 7;
const CHUNK_SIZE: i64 = 50;

#[derive(Parser, Debug)]
#[command(author, version, about = "Backup a directory to Google Cloud Storage", long_about = None)]
struct Args {
    /// Path to the directory to backup
    #[arg(short, long)]
    source: Option<PathBuf>,

    /// GCS bucket name
    #[arg(short, long)]
    bucket: Option<String>,

    /// Path to GCS credentials JSON file
    #[arg(short, long)]
    credentials: Option<PathBuf>,

    /// Destination folder in bucket (optional)
    #[arg(short, long)]
    destination_folder: Option<String>,

    /// Use config file instead of arguments
    #[arg(long, default_value = "backup-config.json")]
    config: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
struct BackupConfig {
    source_path: PathBuf,
    bucket_name: String,
    credentials_path: PathBuf,
    destination_folder: Option<String>,
}

impl BackupConfig {
    fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .context("Failed to read config file")?;
        let config: BackupConfig = serde_json::from_str(&content)
            .context("Failed to parse config file")?;
        Ok(config)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .context("Failed to serialize config")?;
        fs::write(path, json)
            .context("Failed to write config file")?;
        Ok(())
    }
}


fn zip_directory(source_dir: &Path, output_dir: &Path) -> Result<()> {
    // zips directory in chunks locally
    println!("Creating zip archive...");
    println!("Source: {}", source_dir.display());
    println!("Output directory: {}", output_dir.display());

    let options = FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let mut file_count = 0;
    
    // Walk through the directory
    let walkdir = walkdir::WalkDir::new(source_dir);

    // init a dud zip file (I'm sure this can be done better lol)
    let mut zip = ZipWriter::new(Path::new("temp.zip"));
    for entry in walkdir.into_iter().filter_map(|e| e.ok()) {
        // get ZIP chunk filename and init if necessary
        if file_count % CHUNK_SIZE == 0 && file_count == 0 {
            let zip_file_name = if file_count == 0 {
                "0.zip"
            } else {
                // close precious zip file before we init the next one
                zip.finish().context("Failed to finalize zip file")?;
                &format!("{}.zip", file_count / CHUNK_SIZE)
            };
            let mut file = File::create(output_dir.join(zip_file_name)).context("Failed to create zip file")?;
            let mut zip = ZipWriter::new(file);
        }
        
        // now read the file and 
        let path = entry.path();
        let name = path.strip_prefix(source_dir)
            .context("Failed to strip prefix")?;

        // Skip the root directory itself
        if name.as_os_str().is_empty() {
            continue;
        }

        if path.is_file() {
            // read file data into buffer
            let mut f = File::open(path)
                .context("Failed to open file for zipping")?;
            let mut buffer = Vec::new();
            f.read_to_end(&mut buffer)
                .context("Failed to read file")?;

            // open zip file to write new entry
            zip.start_file(name.to_string_lossy().into_owned(), options)
                .context("Failed to start zip file entry")?;
            zip.write_all(&buffer)
                .context("Failed to write to zip")?;
            
            file_count += 1;
        } else if !name.as_os_str().is_empty() {
            zip.add_directory(name.to_string_lossy().into_owned(), options)
                .context("Failed to add directory to zip")?;
        }
    }

    
    println!("Zip created successfully!");
    println!("Files: {}", file_count);
    
    Ok(())
}

async fn upload_to_gcs(
    file_path: &Path,
    bucket_name: &str,
    destination_name: String,
    credentials_path: &Path,
) -> Result<()> {
    // Read credentials
    let creds_content = fs::read_to_string(credentials_path)
        .context("Failed to read credentials file")?;
    let creds: CredentialsFile = serde_json::from_str(&creds_content)
        .context("Failed to parse credentials")?;

    // Create GCS client
    let config = ClientConfig::default()
        .with_credentials(creds)
        .await
        .expect("Failed to create client config");
    let client = Client::new(config);

    // Read file
    let mut file = File::open(file_path)
        .context("Failed to open file for upload")?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .context("Failed to read file")?;

    // Upload to bucket
    let media = Media::new(destination_name.clone());
    let upload_type = UploadType::Simple(media);
    let uploaded = client
        .upload_object(
            &UploadObjectRequest {
                bucket: bucket_name.to_string(),
                ..Default::default()
            },
            buffer,
            &upload_type,
        )
        .await
        .context("Failed to upload to GCS")?;

    println!("Upload successful!");
    println!("Location: gs://{}/{}", bucket_name, uploaded.name);
    
    Ok(())
}

fn cleanup_old_backups(backup_dir: &Path, keep_days: i64) -> Result<()> {
    let cutoff = Local::now() - chrono::Duration::days(keep_days);
    
    if !backup_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(backup_dir)? {
        let entry = entry?;
        let path = entry.path();
        
        if path.extension().and_then(|s| s.to_str()) == Some("zip") {
            let metadata = fs::metadata(&path)?;
            if let Ok(modified) = metadata.modified() {
                let modified_time = chrono::DateTime::<Local>::from(modified);
                if modified_time < cutoff {
                    fs::remove_file(&path)?;
                    println!("Removed old backup: {}", path.file_name().unwrap().to_string_lossy());
                }
            }
        }
    }
    
    Ok(())
}


#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    println!("Starting backup..."); 

    // Load or create config
    let config = if args.config.exists() && args.source.is_none() {
        println!("📋 Loading configuration from {}...", args.config.display());
        BackupConfig::from_file(&args.config)?
    } else {
        // Build config from arguments
        let source = args.source.context("Source path required")?;
        let bucket = args.bucket.context("Bucket name required")?;
        let credentials = args.credentials.context("Credentials path required")?;
        
        let config = BackupConfig {
            source_path: source,
            bucket_name: bucket,
            credentials_path: credentials,
            destination_folder: args.destination_folder,
        };
        
        // Save config for future use
        config.save(&args.config)?;
        println!("✓ Configuration saved to {}", args.config.display());
        
        config
    };

    // Validate paths
    if !config.source_path.exists() {
        anyhow::bail!("Source directory does not exist: {}", config.source_path.display());
    }
    if !config.credentials_path.exists() {
        anyhow::bail!("Credentials file does not exist: {}", config.credentials_path.display());
    }

    // Create temp directory for zips
    let temp_dir = Path::new("tmp");
    fs::create_dir_all(temp_dir)?;

    // Generate filename with timestamp
    let timestamp = Local::now().format("%Y%m%d");
    let source_name = config.source_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("backup");
    let zip_name = format!("{}-{}", source_name, timestamp);
    let local_zip_dir = temp_dir.join(&zip_name);

    // Determine GCS destination path
    let gcs_dir = if let Some(folder) = &config.destination_folder {
        format!("{}/{}", folder, zip_name)
    } else {
        zip_name.clone()
    };

    println!("Source: {}", config.source_path.display());
    println!("Bucket: {}", config.bucket_name);
    println!("Destination: {}\n", gcs_dir);

    // ZIP the file to a temp directory
    zip_directory(&config.source_path, &local_zip_dir)?;

    // Then upload to GCS
    upload_to_gcs(
        &local_zip_dir
        &config.bucket_name,
        &gcs_dir,
        &config.credentials_path,
    ).await?;

    // and cleanup the temp directory
    cleanup_old_backups(temp_dir, KEEP_LOCAL_BACKUP_DAYS)?;
    println!("Backup completed successfully!");

    Ok(())
}

