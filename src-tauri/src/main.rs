// src-tauri/src/main.rs

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use walkdir::WalkDir;
use ini::Ini;
use std::collections::HashMap;
use regex::Regex;
use lazy_static::lazy_static;
use rusqlite::{Connection, OptionalExtension, Result as SqlResult, params};
use serde::{Serialize, Deserialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Arc};
use tauri::{
    command, generate_context, generate_handler, AppHandle, Manager, State, api::dialog,
    api::process::Command, Window
};
use thiserror::Error;
use once_cell::sync::Lazy;
use tauri::async_runtime;
use toml;
use tauri::api::file::read_binary;
use std::io::{Read, Seek, Cursor}; // For reading zip files
use zip::ZipArchive;

// --- Structs for Deserializing Definitions ---
#[derive(Deserialize, Debug, Clone)]
struct EntityDefinition {
    name: String,
    slug: String,
    description: Option<String>,
    details: Option<String>,
    base_image: Option<String>,
}

#[derive(Deserialize, Debug)]
struct CategoryDefinition {
    name: String,
    entities: Vec<EntityDefinition>,
}

// Struct to hold asset info needed for delete/relocate
#[derive(Debug)]
struct AssetLocationInfo {
    id: i64,
    clean_relative_path: String, // Stored relative path (e.g., category/entity/mod_name)
    entity_id: i64,
    category_slug: String,
    entity_slug: String,
}

// Type alias for the top-level structure (HashMap: category_slug -> CategoryDefinition)
type Definitions = HashMap<String, CategoryDefinition>;

// --- Constants for Settings Keys ---
const SETTINGS_KEY_MODS_FOLDER: &str = "mods_folder_path";
const OTHER_ENTITY_SUFFIX: &str = "-other";
const OTHER_ENTITY_NAME: &str = "Other/Unknown";
const DB_NAME: &str = "app_data.sqlite";
const DISABLED_PREFIX: &str = "DISABLED_";
const TARGET_IMAGE_FILENAME: &str = "preview.png";

// --- Error Handling ---
#[derive(Debug, Error)]
enum AppError {
    #[error("Database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization/deserialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Tauri path resolution error: {0}")]
    TauriPath(String),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Mod operation failed: {0}")]
    ModOperation(String),
    #[error("Resource not found: {0}")]
    NotFound(String),
    #[error("Operation cancelled by user")]
    UserCancelled,
    #[error("Shell command failed: {0}")]
    ShellCommand(String),
    #[error("Zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

// --- Event Payload Struct ---
#[derive(Clone, serde::Serialize)]
struct ScanProgress {
  processed: usize,
  total: usize,
  current_path: Option<String>,
  message: String,
}

// --- Event Names ---
const SCAN_PROGRESS_EVENT: &str = "scan://progress";
const SCAN_COMPLETE_EVENT: &str = "scan://complete";
const SCAN_ERROR_EVENT: &str = "scan://error";

type CmdResult<T> = Result<T, String>;

struct DbState(Arc<Mutex<Connection>>);

static DB_CONNECTION: Lazy<Mutex<SqlResult<Connection>>> = Lazy::new(|| {
    Mutex::new(Err(rusqlite::Error::InvalidPath("DB not initialized yet".into())))
});

lazy_static! {
    static ref MOD_NAME_CLEANUP_REGEX: Regex = Regex::new(r"(?i)(_v\d+(\.\d+)*|_DISABLED|DISABLED_|\(disabled\)|^DISABLED_)").unwrap();
    static ref CHARACTER_NAME_REGEX: Regex = Regex::new(r"(?i)(Raiden|Shogun|HuTao|Tao|Zhongli|Ganyu|Ayaka|Kazuha|Yelan|Eula|Klee|Nahida)").unwrap();
}

#[derive(Debug)]
struct DeducedInfo {
    entity_slug: String,
    mod_name: String,
    mod_type_tag: Option<String>,
    author: Option<String>,
    description: Option<String>,
    image_filename: Option<String>,
}

#[derive(Clone)] // Allow cloning for the async task
struct DeductionMaps {
    category_slug_to_id: HashMap<String, i64>,
    entity_slug_to_id: HashMap<String, i64>,
    lowercase_category_name_to_slug: HashMap<String, String>,
    lowercase_entity_name_to_slug: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug)] struct Category { id: i64, name: String, slug: String }
#[derive(Serialize, Deserialize, Debug)] struct Entity { id: i64, category_id: i64, name: String, slug: String, description: Option<String>, details: Option<String>, base_image: Option<String>, mod_count: i32 }
#[derive(Serialize, Deserialize, Debug, Clone)] struct Asset { id: i64, entity_id: i64, name: String, description: Option<String>, folder_name: String, image_filename: Option<String>, author: Option<String>, category_tag: Option<String>, is_enabled: bool }

// Structs for Import/Analysis
#[derive(Serialize, Debug, Clone)]
struct ArchiveEntry {
    path: String,
    is_dir: bool,
    is_likely_mod_root: bool,
}

#[derive(Serialize, Debug, Clone)]
struct ArchiveAnalysisResult {
    file_path: String,
    entries: Vec<ArchiveEntry>,
    deduced_mod_name: Option<String>,
    deduced_author: Option<String>,
    deduced_category_slug: Option<String>, // Keep for potential future backend use
    deduced_entity_slug: Option<String>,   // Keep for potential future backend use
    // --> Added Raw INI fields <--
    raw_ini_type: Option<String>,          // e.g., "Character", "Weapon"
    raw_ini_target: Option<String>,        // e.g., "Nahida", "Raiden Shogun", "Aqua Simulacra"
    // --------------------------
    detected_preview_internal_path: Option<String>,
}

// --- Helper Functions for Deduction ---

fn fetch_deduction_maps(conn: &Connection) -> SqlResult<DeductionMaps> {
    let mut category_slug_to_id = HashMap::new();
    let mut lowercase_category_name_to_slug = HashMap::new();
    let mut cat_stmt = conn.prepare("SELECT slug, id, name FROM categories")?;
    let cat_rows = cat_stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)))?;
    for row in cat_rows {
        if let Ok((slug, id, name)) = row {
            lowercase_category_name_to_slug.insert(name.to_lowercase(), slug.clone());
            category_slug_to_id.insert(slug, id);
        }
    }

    let mut entity_slug_to_id = HashMap::new();
    let mut lowercase_entity_name_to_slug = HashMap::new();
    let mut entity_stmt = conn.prepare("SELECT slug, id, name FROM entities")?;
    let entity_rows = entity_stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)))?;
    for row in entity_rows {
        if let Ok((slug, id, name)) = row {
             // Store original slug for ID mapping
            entity_slug_to_id.insert(slug.clone(), id);
             // Also store lowercase name -> original slug mapping
             // Handle potential name collisions simply by letting the last one win, or log warning
             if lowercase_entity_name_to_slug.contains_key(&name.to_lowercase()) {
                 // Optional: Log collision warning if needed
                 // println!("Warning: Duplicate entity name detected (case-insensitive): {}", name);
             }
             lowercase_entity_name_to_slug.insert(name.to_lowercase(), slug);
        }
    }

    Ok(DeductionMaps {
        category_slug_to_id,
        entity_slug_to_id,
        lowercase_category_name_to_slug,
        lowercase_entity_name_to_slug,
    })
}

fn deduce_mod_info_v2(
    mod_folder_path: &PathBuf,
    base_mods_path: &PathBuf,
    maps: &DeductionMaps,
) -> Option<DeducedInfo> {
    let mod_folder_name = mod_folder_path.file_name()?.to_string_lossy().to_string();
    let mut info = DeducedInfo {
        // Start with a generic 'other' - will be refined
        entity_slug: format!("{}{}", "unknown", OTHER_ENTITY_SUFFIX),
        mod_name: mod_folder_name.clone(), // Initial guess
        mod_type_tag: None,
        author: None,
        description: None,
        image_filename: find_preview_image(mod_folder_path), // Keep existing image finder
    };

    let mut found_entity_slug: Option<String> = None;
    let mut found_category_slug: Option<String> = None; // Track deduced category separately

    // --- 1. Deduce from Parent Folders ---
    let mut current_path = mod_folder_path.parent();
    while let Some(path) = current_path {
        // Stop if we reached the base mods path or its parent
        if path == *base_mods_path || path.parent() == Some(base_mods_path) {
            break;
        }
        if let Some(folder_name) = path.file_name().and_then(|n| n.to_str()) {
            let lower_folder_name = folder_name.to_lowercase();

            // Check Entity Slug/Name (only if entity not already found)
            if found_entity_slug.is_none() {
                if maps.entity_slug_to_id.contains_key(folder_name) {
                    found_entity_slug = Some(folder_name.to_string());
                     println!("[DeduceV2 {}] Found entity slug '{}' from parent folder.", mod_folder_name, found_entity_slug.as_ref().unwrap());
                    // Don't break yet, continue checking for category higher up
                } else if let Some(slug) = maps.lowercase_entity_name_to_slug.get(&lower_folder_name) {
                    found_entity_slug = Some(slug.clone());
                     println!("[DeduceV2 {}] Found entity '{}' (name match) from parent folder.", mod_folder_name, found_entity_slug.as_ref().unwrap());
                    // Don't break yet
                }
            }

            // Check Category Slug/Name (always check, store the highest-level match)
            if maps.category_slug_to_id.contains_key(folder_name) {
                found_category_slug = Some(folder_name.to_string());
                 println!("[DeduceV2 {}] Found category slug '{}' from parent folder (higher priority).", mod_folder_name, found_category_slug.as_ref().unwrap());
            } else if let Some(slug) = maps.lowercase_category_name_to_slug.get(&lower_folder_name) {
                 // Only update if we haven't found a slug match higher up
                if found_category_slug.is_none() || !maps.category_slug_to_id.contains_key(found_category_slug.as_ref().unwrap()) {
                     found_category_slug = Some(slug.clone());
                     println!("[DeduceV2 {}] Found category '{}' (name match) from parent folder.", mod_folder_name, found_category_slug.as_ref().unwrap());
                 }
            }
        }
        current_path = path.parent(); // Move up one level
    } // End parent folder walk

    // --- 2. Deduce from .ini File (Overrides folder names if specific) ---
    let mut ini_target_hint: Option<String> = None;
    let mut ini_type_hint: Option<String> = None;

    // Find the first .ini file directly inside the mod folder
    let ini_path_option = WalkDir::new(mod_folder_path)
        .max_depth(1) // Only immediate children
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|entry| entry.file_type().is_file() && entry.path().extension().map_or(false, |ext| ext.eq_ignore_ascii_case("ini")))
        .map(|e| e.into_path());

    if let Some(ini_path) = ini_path_option {
        if let Ok(ini_content) = fs::read_to_string(&ini_path) {
            if let Ok(ini) = Ini::load_from_str(&ini_content) {
                for section_name in ["Mod", "Settings", "Info", "General"] {
                    if let Some(section) = ini.section(Some(section_name)) {
                        // Get Name, Author, Desc (can override defaults)
                        if let Some(name) = section.get("Name").or_else(|| section.get("ModName")) { info.mod_name = name.trim().to_string(); }
                        if let Some(author) = section.get("Author") { info.author = Some(author.trim().to_string()); }
                        if let Some(desc) = section.get("Description") { info.description = Some(desc.trim().to_string()); }
                        // Get Hints (don't override folder deduction yet)
                        if let Some(target) = section.get("Target").or_else(|| section.get("Entity")).or_else(|| section.get("Character")) { ini_target_hint = Some(target.trim().to_string()); }
                        if let Some(typ) = section.get("Type").or_else(|| section.get("Category")) { ini_type_hint = Some(typ.trim().to_string()); info.mod_type_tag = Some(typ.trim().to_string()); } // Also store raw tag
                        // Break early if we found hints? Maybe not, let last section win.
                    }
                }
            }
        }
    }

    // Try matching INI Target Hint (if entity not already found via folders)
    if found_entity_slug.is_none() {
        if let Some(target) = ini_target_hint {
            let lower_target = target.to_lowercase();
            if maps.entity_slug_to_id.contains_key(&target) {
                found_entity_slug = Some(target);
                println!("[DeduceV2 {}] Found entity slug '{}' from ini target.", mod_folder_name, found_entity_slug.as_ref().unwrap());
            } else if let Some(slug) = maps.lowercase_entity_name_to_slug.get(&lower_target) {
                found_entity_slug = Some(slug.clone());
                 println!("[DeduceV2 {}] Found entity '{}' (name match) from ini target.", mod_folder_name, found_entity_slug.as_ref().unwrap());
            }
        }
    }

    // Try matching INI Type Hint (if category not already found via folders)
    if found_category_slug.is_none() {
        if let Some(typ) = ini_type_hint {
            let lower_typ = typ.to_lowercase();
             if maps.category_slug_to_id.contains_key(&typ) {
                found_category_slug = Some(typ);
                 println!("[DeduceV2 {}] Found category slug '{}' from ini type.", mod_folder_name, found_category_slug.as_ref().unwrap());
            } else if let Some(slug) = maps.lowercase_category_name_to_slug.get(&lower_typ) {
                found_category_slug = Some(slug.clone());
                println!("[DeduceV2 {}] Found category '{}' (name match) from ini type.", mod_folder_name, found_category_slug.as_ref().unwrap());
            }
        }
    }


    // --- 3. Final Assignment Logic ---
    if let Some(entity_slug) = found_entity_slug {
        // Entity found, assign it directly
        info.entity_slug = entity_slug;
        // We don't strictly *need* the category slug now, but could look it up if needed elsewhere
    } else if let Some(category_slug) = found_category_slug {
        // No specific entity, but category found. Assign to category's 'other'.
        info.entity_slug = format!("{}{}", category_slug, OTHER_ENTITY_SUFFIX);
         println!("[DeduceV2 {}] No specific entity, assigned to category other: '{}'", mod_folder_name, info.entity_slug);
    } else {
         // Fallback to default (e.g., characters-other or a generic unknown)
         let fallback_category = "characters"; // Or choose a better default
         info.entity_slug = format!("{}{}", fallback_category, OTHER_ENTITY_SUFFIX);
         println!("[DeduceV2 {}] No entity or category deduced, assigned to fallback: '{}'", mod_folder_name, info.entity_slug);
    }

    // Clean up Mod Name
    info.mod_name = MOD_NAME_CLEANUP_REGEX.replace_all(&info.mod_name, "").trim().to_string();
    if info.mod_name.is_empty() { // Use original folder name if cleanup resulted in empty
        info.mod_name = mod_folder_name;
    }

    Some(info)
}

fn get_asset_location_info(conn: &Connection, asset_id: i64) -> Result<AssetLocationInfo, AppError> {
    conn.query_row(
        "SELECT a.id, a.folder_name, a.entity_id, c.slug, e.slug
         FROM assets a
         JOIN entities e ON a.entity_id = e.id
         JOIN categories c ON e.category_id = c.id
         WHERE a.id = ?1",
        params![asset_id],
        |row| {
            Ok(AssetLocationInfo {
                id: row.get(0)?,
                // Ensure forward slashes when reading
                clean_relative_path: row.get::<_, String>(1)?.replace("\\", "/"),
                entity_id: row.get(2)?,
                category_slug: row.get(3)?,
                entity_slug: row.get(4)?,
            })
        }
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => AppError::NotFound(format!("Asset with ID {} not found", asset_id)),
        _ => AppError::Sqlite(e),
    })
}

fn has_ini_file(dir_path: &PathBuf) -> bool {
    if !dir_path.is_dir() { return false; }
    // Use walkdir limited to depth 1 to avoid iterating too deep if not needed
    for entry in WalkDir::new(dir_path).max_depth(1).min_depth(1).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            if let Some(ext) = entry.path().extension() {
                if ext.to_ascii_lowercase() == "ini" {
                    return true;
                }
            }
        }
    }
    false
}

fn find_preview_image(dir_path: &PathBuf) -> Option<String> {
    let common_names = ["preview.png", "preview.jpg", "icon.png", "icon.jpg", "thumbnail.png", "thumbnail.jpg"];
     if !dir_path.is_dir() { return None; }
    // Use walkdir limited to depth 1
    for entry in WalkDir::new(dir_path).max_depth(1).min_depth(1).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
             if let Some(filename) = entry.path().file_name().and_then(|n| n.to_str()) {
                 if common_names.contains(&filename.to_lowercase().as_str()) {
                     return Some(filename.to_string());
                 }
             }
        }
    }
    None
}

// --- Database Initialization (Result type uses AppError internally) ---
fn initialize_database(app_handle: &AppHandle) -> Result<(), AppError> {
    let data_dir = get_app_data_dir(app_handle)?;
    if !data_dir.exists() {
        fs::create_dir_all(&data_dir)?;
    }
    let db_path = data_dir.join(DB_NAME);
    println!("Database path: {}", db_path.display());
    let conn = Connection::open(&db_path)?;

    // --- Create Tables ---
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS categories ( id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE NOT NULL, slug TEXT UNIQUE NOT NULL );
         CREATE TABLE IF NOT EXISTS entities ( id INTEGER PRIMARY KEY AUTOINCREMENT, category_id INTEGER NOT NULL, name TEXT NOT NULL, slug TEXT UNIQUE NOT NULL, description TEXT, details TEXT, base_image TEXT, FOREIGN KEY (category_id) REFERENCES categories (id) );
         -- Removed UNIQUE constraint from folder_name temporarily, need better handling for duplicates during scan
         CREATE TABLE IF NOT EXISTS assets ( id INTEGER PRIMARY KEY AUTOINCREMENT, entity_id INTEGER NOT NULL, name TEXT NOT NULL, description TEXT, folder_name TEXT NOT NULL, image_filename TEXT, author TEXT, category_tag TEXT, FOREIGN KEY (entity_id) REFERENCES entities (id) );
         CREATE TABLE IF NOT EXISTS settings ( key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL );
         COMMIT;",
    )?;
    println!("Database tables verified/created.");

    // --- Load and Parse Definitions ---
    println!("Loading base entity definitions...");
    // Embed the TOML file content at compile time
    let definitions_toml_str = include_str!("../definitions/base_entities.toml");
    let definitions: Definitions = toml::from_str(definitions_toml_str)
        .map_err(|e| AppError::Config(format!("Failed to parse base_entities.toml: {}", e)))?;
    println!("Loaded {} categories from definitions.", definitions.len());


    // --- Populate Database from Definitions ---
    println!("Populating database from definitions...");
    let mut categories_processed = 0;
    let mut entities_processed = 0;

    for (category_slug, category_def) in definitions.iter() {
        // 1. Insert Category (Ignore if exists)
        let cat_insert_res = conn.execute(
            "INSERT OR IGNORE INTO categories (name, slug) VALUES (?1, ?2)",
            params![category_def.name, category_slug],
        );
        if let Err(e) = cat_insert_res {
             eprintln!("Error inserting category '{}': {}", category_slug, e);
             continue; // Skip this category if insert fails critically
        }
        categories_processed += 1;

        // 2. Get Category ID (must exist now)
        let category_id: i64 = conn.query_row(
            "SELECT id FROM categories WHERE slug = ?1",
            params![category_slug],
            |row| row.get(0),
        ).map_err(|e| AppError::Config(format!("Failed to get category ID for '{}': {}", category_slug, e)))?;

        // 3. Ensure "Other" Entity for this Category
        let other_slug = format!("{}{}", category_slug, OTHER_ENTITY_SUFFIX);
        conn.execute(
            "INSERT OR IGNORE INTO entities (category_id, name, slug, description, details, base_image)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ category_id, OTHER_ENTITY_NAME, other_slug, "Uncategorized assets.", "{}", None::<String> ]
        ).map_err(|e| AppError::Config(format!("Failed to insert 'Other' entity for category '{}': {}", category_slug, e)))?;


        // 4. Insert Entities defined in TOML (Ignore if exists based on slug)
        for entity_def in category_def.entities.iter() {
            let ent_insert_res = conn.execute(
                 "INSERT OR IGNORE INTO entities (category_id, name, slug, description, details, base_image)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                 params![
                     category_id,
                     entity_def.name,
                     entity_def.slug,
                     entity_def.description,
                     entity_def.details.as_ref().map(|s| s.to_string()).unwrap_or("{}".to_string()), // Default to empty JSON string if None
                     entity_def.base_image,
                 ]
            );
             if let Err(e) = ent_insert_res {
                 eprintln!("Error inserting entity '{}' for category '{}': {}", entity_def.slug, category_slug, e);
                 // Continue to next entity even if one fails
             } else {
                  entities_processed += 1; // Count attempted inserts
             }
        }
    }
    println!("Finished populating. Processed {} categories and {} entities from definitions.", categories_processed, entities_processed);

    // --- Finalize DB Connection Setup for State ---
    let mut db_lock = DB_CONNECTION.lock().expect("Failed to lock DB mutex during init");
    *db_lock = Ok(conn); // Store the connection we used

    println!("Database initialization and definition sync complete.");
    Ok(())
}

// --- Utility Functions ---
fn get_app_data_dir(app_handle: &AppHandle) -> Result<PathBuf, AppError> { // Internal error type
    app_handle.path_resolver()
        .app_data_dir()
        .ok_or_else(|| AppError::TauriPath("Failed to resolve app data directory".to_string()))
}

// Helper to get a setting value (Internal error type)
fn get_setting_value(conn: &Connection, key: &str) -> Result<Option<String>, AppError> { // Internal error type
    let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
    let result = stmt.query_row(params![key], |row| row.get(0)).optional()?;
    Ok(result)
}

// Helper to get the configured mods base path (Internal error type)
fn get_mods_base_path_from_settings(db_state: &DbState) -> Result<PathBuf, AppError> { // Internal error type
    let conn = db_state.0.lock().map_err(|_| AppError::Config("DB lock poisoned".into()))?;
    get_setting_value(&conn, SETTINGS_KEY_MODS_FOLDER)?
        .map(PathBuf::from)
        .ok_or_else(|| AppError::Config("Mods folder path not set".to_string()))
}

// Helper to get entity mods path using settings (Internal error type)
// FIX: Removed unused app_handle parameter
fn get_entity_mods_path(db_state: &DbState, entity_slug: &str) -> Result<PathBuf, AppError> {
    let base_path = get_mods_base_path_from_settings(db_state)?;
    Ok(base_path.join(entity_slug))
}

// --- Tauri Commands (Return CmdResult<T> = Result<T, String>) ---

// == Settings Commands ==

#[command]
fn get_setting(key: String, db_state: State<DbState>) -> CmdResult<Option<String>> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    get_setting_value(&conn, &key).map_err(|e| e.to_string()) // Convert internal error to string
}

#[command]
fn set_setting(key: String, value: String, db_state: State<DbState>) -> CmdResult<()> { // Returns Result<(), String>
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
        params![key, value],
    ).map_err(|e| e.to_string())?; // Convert error
    println!("Set setting '{}' to '{}'", key, value);
    Ok(())
}

#[command]
async fn select_directory() -> CmdResult<Option<PathBuf>> { // Removed AppHandle
    // FIX: Remove AppHandle from new(), use blocking dialog directly
    let result = dialog::blocking::FileDialogBuilder::new()
        .set_title("Select Mods Folder")
        .pick_folder();

    match result {
        Some(path) => Ok(Some(path)),
        None => Ok(None), // User cancelled
    }
}

#[command]
async fn select_file() -> CmdResult<Option<PathBuf>> { // Removed AppHandle
    // FIX: Use add_filter instead of dialog::Filter struct
    let result = dialog::blocking::FileDialogBuilder::new() // FIX: Remove AppHandle
        .set_title("Select Quick Launch Executable")
        .add_filter("Executable", &["exe", "bat", "cmd", "sh", "app"]) // FIX: Use add_filter
        .add_filter("All Files", &["*"]) // FIX: Use add_filter
        .pick_file();

    match result {
        Some(path) => Ok(Some(path)),
        None => Ok(None), // User cancelled
    }
}

#[command]
async fn launch_executable(path: String, _app_handle: AppHandle) -> CmdResult<()> { // app_handle might not be needed now
    println!("Attempting to launch via Command::new: {}", path);

    // FIX: Use Command::new for launching executables
    let cmd = Command::new(path) // Use the path directly as the command
        // .args([]) // Add arguments if needed later
        .spawn(); // Spawn the process

    match cmd {
        Ok((mut rx, _child)) => {
            // You can optionally read stdout/stderr here if needed
             while let Some(event) = rx.recv().await {
                 match event {
                    tauri::api::process::CommandEvent::Stdout(line) => {
                        println!("Launcher stdout: {}", line);
                    }
                    tauri::api::process::CommandEvent::Stderr(line) => {
                        eprintln!("Launcher stderr: {}", line);
                    }
                    tauri::api::process::CommandEvent::Error(e) => {
                         eprintln!("Launcher error event: {}", e);
                         // Decide if this constitutes a failure
                         // return Err(format!("Launcher process event error: {}", e));
                    }
                     tauri::api::process::CommandEvent::Terminated(payload) => {
                        println!("Launcher terminated: {:?}", payload);
                        if let Some(code) = payload.code {
                             if code != 0 {
                                println!("Launcher exited with non-zero code: {}", code);
                                // Optionally return error based on exit code
                                // return Err(format!("Launcher exited with code {}", code));
                             }
                         } else {
                             println!("Launcher terminated without exit code (possibly killed).");
                         }
                         // Process terminated, break the loop
                         break;
                     }
                    _ => {} // Ignore other events like Terminated
                }
             }
             println!("Launcher process finished or detached.");
             Ok(()) // Assume success if spawn worked and process finished/detached
        }
        Err(e) => {
            eprintln!("Failed to spawn launcher: {}", e);
            Err(format!("Failed to spawn executable: {}", e)) // Convert error to string
        }
    }
}


// == Core Commands (Return CmdResult<T>) ==

#[command]
fn get_categories(db_state: State<DbState>) -> CmdResult<Vec<Category>> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let mut stmt = conn.prepare("SELECT id, name, slug FROM categories ORDER BY name")
        .map_err(|e| e.to_string())?; // Convert error
    let category_iter = stmt.query_map([], |row| {
        Ok(Category {
            id: row.get(0)?, name: row.get(1)?, slug: row.get(2)?,
        })
    }).map_err(|e| e.to_string())?; // Convert error
    category_iter.collect::<SqlResult<Vec<Category>>>().map_err(|e| e.to_string()) // Convert error
}

#[command]
fn get_category_entities(category_slug: String, db_state: State<DbState>) -> CmdResult<Vec<Entity>> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
     let category_id: i64 = conn.query_row(
        "SELECT id FROM categories WHERE slug = ?1",
        params![category_slug],
        |row| row.get(0),
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => format!("Category '{}' not found", category_slug),
        _ => e.to_string(),
    })?;

     // Fetch id, name, slug - ORDER BY to put 'Other' first
     let mut stmt = conn.prepare(
        "SELECT id, name, slug
         FROM entities
         WHERE category_id = ?1
         ORDER BY
            CASE WHEN slug LIKE '%-other' THEN 0 ELSE 1 END ASC,
            name ASC"
    ).map_err(|e| e.to_string())?; // Corrected SQL query

    let entity_iter = stmt.query_map(params![category_id], |row| {
        Ok(Entity {
            id: row.get(0)?,
            category_id: category_id,
            name: row.get(1)?,
            slug: row.get(2)?,
            description: None,
            details: None,
            base_image: None,
            mod_count: 0
        })
    }).map_err(|e| e.to_string())?;
    entity_iter.collect::<SqlResult<Vec<Entity>>>().map_err(|e| e.to_string())
}

#[command]
fn get_entities_by_category(category_slug: String, db_state: State<DbState>) -> CmdResult<Vec<Entity>> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
     let category_id: i64 = conn.query_row(
        "SELECT id FROM categories WHERE slug = ?1",
        params![category_slug],
        |row| row.get(0),
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => format!("Category '{}' not found", category_slug),
        _ => e.to_string(),
    })?;

     // Fetch full entity details - ORDER BY to put 'Other' first
     let mut stmt = conn.prepare(
        "SELECT e.id, e.category_id, e.name, e.slug, e.description, e.details, e.base_image, COUNT(a.id) as mod_count
         FROM entities e LEFT JOIN assets a ON e.id = a.entity_id
         WHERE e.category_id = ?1
         GROUP BY e.id
         ORDER BY
            CASE WHEN e.slug LIKE '%-other' THEN 0 ELSE 1 END ASC,
            e.name ASC" // Corrected SQL query
    ).map_err(|e| e.to_string())?;

    let entity_iter = stmt.query_map(params![category_id], |row| {
        Ok(Entity {
            id: row.get(0)?, category_id: row.get(1)?, name: row.get(2)?,
            slug: row.get(3)?, description: row.get(4)?, details: row.get(5)?,
            base_image: row.get(6)?, mod_count: row.get(7)?
        })
    }).map_err(|e| e.to_string())?;
    entity_iter.collect::<SqlResult<Vec<Entity>>>().map_err(|e| e.to_string())
}


#[command]
fn get_entity_details(entity_slug: String, db_state: State<DbState>) -> CmdResult<Entity> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
     let mut stmt = conn.prepare(
        "SELECT e.id, e.category_id, e.name, e.slug, e.description, e.details, e.base_image, COUNT(a.id) as mod_count
         FROM entities e LEFT JOIN assets a ON e.id = a.entity_id
         WHERE e.slug = ?1 GROUP BY e.id"
    ).map_err(|e| e.to_string())?;
    stmt.query_row(params![entity_slug], |row| {
         Ok(Entity {
            id: row.get(0)?, category_id: row.get(1)?, name: row.get(2)?,
            slug: row.get(3)?, description: row.get(4)?, details: row.get(5)?,
            base_image: row.get(6)?, mod_count: row.get(7)?
        })
    }).map_err(|e| match e { // Map specific internal errors to String
        rusqlite::Error::QueryReturnedNoRows => format!("Entity '{}' not found", entity_slug),
        _ => e.to_string(),
    })
}

#[command]
fn get_assets_for_entity(entity_slug: String, db_state: State<DbState>, _app_handle: AppHandle) -> CmdResult<Vec<Asset>> {
    println!("[get_assets_for_entity {}] Start command.", entity_slug);

    let base_mods_path = get_mods_base_path_from_settings(&db_state)
                             .map_err(|e| format!("[get_assets_for_entity {}] Error getting base mods path: {}", entity_slug, e))?;
    println!("[get_assets_for_entity {}] Base mods path: {}", entity_slug, base_mods_path.display());

    let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let conn = &*conn_guard;
    println!("[get_assets_for_entity {}] DB lock acquired for asset query.", entity_slug);

    // --- Entity ID Lookup ---
    let entity_id: i64 = conn.query_row(
        "SELECT id FROM entities WHERE slug = ?1",
        params![entity_slug],
        |row| row.get(0),
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => format!("[get_assets_for_entity {}] Entity not found for assets lookup", entity_slug),
        _ => format!("[get_assets_for_entity {}] DB Error getting entity ID: {}", entity_slug, e),
    })?;
    println!("[get_assets_for_entity {}] Found entity ID: {}", entity_slug, entity_id);

    // --- Prepare Statement ---
    let mut stmt = conn.prepare(
        "SELECT id, entity_id, name, description, folder_name, image_filename, author, category_tag
         FROM assets WHERE entity_id = ?1 ORDER BY name"
    ).map_err(|e| format!("[get_assets_for_entity {}] DB Error preparing asset statement: {}", entity_slug, e))?;
    println!("[get_assets_for_entity {}] Prepared asset statement.", entity_slug);

    // --- Query Rows ---
    let asset_rows_result = stmt.query_map(params![entity_id], |row| {
        let folder_name_raw: String = row.get(4)?;
        Ok(Asset {
            id: row.get(0)?,
            entity_id: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            // Store the CLEAN relative path from DB directly for now
            folder_name: folder_name_raw.replace("\\", "/"),
            image_filename: row.get(5)?,
            author: row.get(6)?,
            category_tag: row.get(7)?,
            is_enabled: false, // Default, will be determined below
        })
    });

    let mut assets_to_return = Vec::new();
    println!("[get_assets_for_entity {}] Starting iteration over asset rows from DB...", entity_slug);

    match asset_rows_result {
        Ok(asset_iter) => {
             for (index, asset_result) in asset_iter.enumerate() {
                 println!("[get_assets_for_entity {}] Processing asset row index: {}", entity_slug, index);
                 match asset_result {
                     Ok(mut asset_from_db) => {
                         // --- Corrected State Detection Logic ---
                         // `asset_from_db.folder_name` currently holds the CLEAN relative path from DB
                         let clean_relative_path_from_db = PathBuf::from(&asset_from_db.folder_name);
                         println!("[get_assets_for_entity {}] Asset from DB: ID={}, Name='{}', Clean RelPath='{}'", entity_slug, asset_from_db.id, asset_from_db.name, clean_relative_path_from_db.display());

                         // Construct potential paths based on the CLEAN relative path
                         let filename_osstr = clean_relative_path_from_db.file_name().unwrap_or_default();
                         let filename_str = filename_osstr.to_string_lossy();
                         if filename_str.is_empty() {
                             println!("[get_assets_for_entity {}] WARN: Cannot get filename from clean relative path '{}'. Skipping asset ID {}.", entity_slug, clean_relative_path_from_db.display(), asset_from_db.id);
                             continue;
                         }
                         let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
                         let relative_parent_path = clean_relative_path_from_db.parent();

                         // Path if enabled = base / clean_relative_path
                         let full_path_if_enabled = base_mods_path.join(&clean_relative_path_from_db);

                         // Path if disabled = base / relative_parent / disabled_filename
                         let full_path_if_disabled = match relative_parent_path {
                            Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
                            _ => base_mods_path.join(&disabled_filename), // No parent or parent is root
                         };

                         println!("[get_assets_for_entity {}] Checking enabled path: {}", entity_slug, full_path_if_enabled.display());
                         println!("[get_assets_for_entity {}] Checking disabled path: {}", entity_slug, full_path_if_disabled.display());

                         // Determine state based on which path exists
                         if full_path_if_enabled.is_dir() {
                             asset_from_db.is_enabled = true;
                             // Set folder_name to the actual path found on disk
                             asset_from_db.folder_name = clean_relative_path_from_db.to_string_lossy().replace("\\", "/");
                             println!("[get_assets_for_entity {}] Mod state determined: ENABLED. Actual disk folder name: {}", entity_slug, asset_from_db.folder_name);
                         } else if full_path_if_disabled.is_dir() {
                             asset_from_db.is_enabled = false;
                             // Set folder_name to the actual path found on disk (the disabled one)
                              let disabled_relative_path = match relative_parent_path {
                                 Some(parent) if parent.as_os_str().len() > 0 => parent.join(&disabled_filename),
                                 _ => PathBuf::from(&disabled_filename),
                              };
                             asset_from_db.folder_name = disabled_relative_path.to_string_lossy().replace("\\", "/");
                             println!("[get_assets_for_entity {}] Mod state determined: DISABLED. Actual disk folder name: {}", entity_slug, asset_from_db.folder_name);
                         } else {
                             // Mod folder doesn't exist in either state
                             println!("[get_assets_for_entity {}] WARN: Mod folder for asset ID {} not found on disk (checked {} and {}). Skipping asset.", entity_slug, asset_from_db.id, full_path_if_enabled.display(), full_path_if_disabled.display());
                             continue; // Skip this asset
                         }

                         println!("[get_assets_for_entity {}] Pushing valid asset to results: ID={}, Name='{}', Folder='{}', Enabled={}",
                                  entity_slug, asset_from_db.id, asset_from_db.name, asset_from_db.folder_name, asset_from_db.is_enabled);
                         assets_to_return.push(asset_from_db);
                         // --- End Corrected State Detection ---
                     }
                     Err(e) => {
                         eprintln!("[get_assets_for_entity {}] Error processing asset row index {}: {}", entity_slug, index, e);
                     }
                 }
             }
             println!("[get_assets_for_entity {}] Finished iterating over asset rows.", entity_slug);
        }
        Err(e) => {
             let err_msg = format!("[get_assets_for_entity {}] DB Error preparing asset iterator: {}", entity_slug, e);
             eprintln!("{}", err_msg);
             return Err(err_msg);
        }
    }

    println!("[get_assets_for_entity {}] Command finished successfully. Returning {} assets.", entity_slug, assets_to_return.len());
    Ok(assets_to_return)
}

#[command]
fn toggle_asset_enabled(entity_slug: String, asset: Asset, db_state: State<DbState>) -> CmdResult<bool> {
    // Note: asset.folder_name passed from frontend is the CURRENT name on disk.
    // We use the asset.id to get the CLEAN relative path from DB for robust path construction.
    println!("[toggle_asset_enabled] Toggling asset: ID={}, Name={}, UI Folder='{}', UI Enabled State={}", asset.id, asset.name, asset.folder_name, asset.is_enabled);

    // Get BASE mods path
    let base_mods_path = get_mods_base_path_from_settings(&db_state).map_err(|e| e.to_string())?;

    // Fetch the CLEAN STORED relative path from DB using asset ID
    let clean_relative_path_from_db_str = {
         let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
         conn.query_row::<String, _, _>(
            "SELECT folder_name FROM assets WHERE id = ?1", // Expecting clean path here
            params![asset.id],
            |row| row.get(0),
         ).map_err(|e| format!("Failed to get relative path from DB for asset ID {}: {}", asset.id, e))?
    };
     // Ensure forward slashes for PathBuf consistency
     let clean_relative_path_from_db_str = clean_relative_path_from_db_str.replace("\\", "/");
     let clean_relative_path_from_db = PathBuf::from(&clean_relative_path_from_db_str);
     println!("[toggle_asset_enabled] Clean relative path from DB: '{}'", clean_relative_path_from_db.display());


    // --- FIX: Construct potential paths correctly ---
    let filename_osstr = clean_relative_path_from_db.file_name().ok_or_else(|| format!("Could not extract filename from DB path: {}", clean_relative_path_from_db.display()))?;
    let filename_str = filename_osstr.to_string_lossy();
    if filename_str.is_empty() {
        return Err(format!("Filename extracted from DB path is empty: {}", clean_relative_path_from_db.display()));
    }
    let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
    let relative_parent_path = clean_relative_path_from_db.parent();

    // Full path if enabled = base / clean_relative_path
    let full_path_if_enabled = base_mods_path.join(&clean_relative_path_from_db);

    // Full path if disabled = base / relative_parent / disabled_filename
    let full_path_if_disabled = match relative_parent_path {
       Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
       _ => base_mods_path.join(&disabled_filename), // No parent or parent is root
    };

    println!("[toggle_asset_enabled] Constructed enabled path check: {}", full_path_if_enabled.display());
    println!("[toggle_asset_enabled] Constructed disabled path check: {}", full_path_if_disabled.display());


    // Determine the CURRENT full path and the TARGET full path based on the *actual* state on disk
    let (current_full_path, target_full_path, new_enabled_state) =
        if full_path_if_enabled.is_dir() { // Check if the ENABLED path exists
            // It's currently enabled on disk, target is the disabled path
             println!("[toggle_asset_enabled] Detected state on disk: ENABLED (found {})", full_path_if_enabled.display());
            (full_path_if_enabled, full_path_if_disabled, false) // New state will be disabled
        } else if full_path_if_disabled.is_dir() { // Check if the DISABLED path exists
            // It's currently disabled on disk, target is the enabled path
             println!("[toggle_asset_enabled] Detected state on disk: DISABLED (found {})", full_path_if_disabled.display());
            (full_path_if_disabled, full_path_if_enabled, true) // New state will be enabled
        } else {
            // Neither exists, something is wrong. Error based on DB path.
             println!("[toggle_asset_enabled] Error: Mod folder not found on disk based on DB relative path!");
            // Use the better error message from before
             return Err(format!(
                "Cannot toggle mod '{}': Folder not found at expected locations derived from DB path '{}' (Checked {} and {}). Did the folder get moved or deleted?",
                asset.name, // Use the display name from the asset object
                clean_relative_path_from_db.display(), // Show the clean path we checked against
                full_path_if_enabled.display(),
                full_path_if_disabled.display()
            ));
        };

    println!("[toggle_asset_enabled] Current actual path: {}", current_full_path.display());
    println!("[toggle_asset_enabled] Target path for rename: {}", target_full_path.display());

    // Perform the rename
    fs::rename(&current_full_path, &target_full_path)
        .map_err(|e| format!("Failed to rename '{}' to '{}': {}", current_full_path.display(), target_full_path.display(), e))?;

    println!("[toggle_asset_enabled] Renamed successfully. New logical state should be: {}", new_enabled_state);

    // Return the actual NEW state after the rename
    Ok(new_enabled_state)
}


#[command]
fn get_asset_image_path(
    _entity_slug: String, // Mark unused, not needed if we have actual folder name
    folder_name_on_disk: String, // The current name on disk (e.g., "ModName" or "DISABLED_ModName")
    image_filename: String,
    db_state: State<DbState> // Need db_state to get base path
) -> CmdResult<String> {
    // Log with relevant info
    println!("[get_asset_image_path] Getting image '{}' from disk folder '{}'", image_filename, folder_name_on_disk);

    // Get the base path from settings
    let base_mods_path = get_mods_base_path_from_settings(&db_state).map_err(|e| e.to_string())?;

    // Construct the FULL path to the mod folder using the name ON DISK
    // This assumes folder_name_on_disk is just the final component.
    let mod_folder_full_path = base_mods_path.join(&folder_name_on_disk);
    println!("[get_asset_image_path] Checking mod folder path: {}", mod_folder_full_path.display());


    // Check if the folder itself exists before looking for the image inside
    if !mod_folder_full_path.is_dir() {
        return Err(format!("Mod folder '{}' not found at expected location: {}", folder_name_on_disk, mod_folder_full_path.display()));
    }

    // Construct the FULL path to the image file
    let image_full_path = mod_folder_full_path.join(&image_filename);
    println!("[get_asset_image_path] Checking image path: {}", image_full_path.display());

    if !image_full_path.is_file() {
        return Err(format!("Image file '{}' not found in mod folder '{}'. Searched: {}", image_filename, folder_name_on_disk, image_full_path.display()));
    }

    // Return the absolute path string for the frontend
    Ok(image_full_path.to_string_lossy().into_owned())
}

#[command]
fn open_mods_folder(_app_handle: AppHandle, db_state: State<DbState>) -> CmdResult<()> { // Mark app_handle unused
    let mods_path = get_mods_base_path_from_settings(&db_state).map_err(|e| e.to_string())?;
    println!("Opening mods folder: {}", mods_path.display());

    if !mods_path.exists() || !mods_path.is_dir() { // Check it's a directory
        eprintln!("Configured mods folder does not exist or is not a directory: {}", mods_path.display());
        return Err(format!("Configured mods folder does not exist or is not a directory: {}", mods_path.display()));
    }

    let command_name;
    let arg; // Variable to hold the single argument string

    // Determine OS-specific command and prepare the argument
    if cfg!(target_os = "windows") {
        command_name = "explorer";
        // Windows explorer doesn't always handle forward slashes well, especially in UNC paths, canonicalize might help sometimes
        // Or just ensure it's a string representation
         arg = mods_path.to_string_lossy().to_string();
    } else if cfg!(target_os = "macos") {
        command_name = "open";
         arg = mods_path.to_str().ok_or("Invalid path string for macOS")?.to_string();
    } else { // Assume Linux/Unix-like
        command_name = "xdg-open";
         arg = mods_path.to_str().ok_or("Invalid path string for Linux")?.to_string();
    }

    println!("Executing: {} \"{}\"", command_name, arg); // Log with quotes for clarity

    // FIX: Use .args() with a slice containing the single argument
    match Command::new(command_name).args(&[arg]).spawn() {
        Ok((_, _child)) => {
             println!("File explorer command spawned successfully.");
             Ok(())
        },
        Err(e) => {
             eprintln!("Failed to spawn file explorer command '{}': {}", command_name, e);
             Err(format!("Failed to open folder using '{}': {}", command_name, e))
        }
    }
}

#[command]
async fn scan_mods_directory(db_state: State<'_, DbState>, app_handle: AppHandle) -> CmdResult<()> {
    println!("Starting robust mod directory scan...");
    let base_mods_path = get_mods_base_path_from_settings(&db_state).map_err(|e| e.to_string())?;
    println!("Scanning base path: {}", base_mods_path.display());

    if !base_mods_path.is_dir() {
        let err_msg = format!("Mods directory path is not a valid directory: {}", base_mods_path.display());
        app_handle.emit_all(SCAN_ERROR_EVENT, &err_msg).unwrap_or_else(|e| eprintln!("Failed to emit scan error event: {}", e));
        return Err(err_msg);
    }

    // --- Preparation ---
    // Pre-fetch maps using the incoming connection *before* spawning task
    let deduction_maps = {
        let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
        let conn = &*conn_guard;
        fetch_deduction_maps(conn).map_err(|e| format!("Failed to pre-fetch deduction maps: {}", e))?
    };
    println!("[Scan Prep] Deduction maps loaded ({} cats, {} entities)", deduction_maps.category_slug_to_id.len(), deduction_maps.entity_slug_to_id.len());

    // --- Get DB Path and Clone necessary data for the task ---
    let db_path = {
        let data_dir = get_app_data_dir(&app_handle).map_err(|e| e.to_string())?;
        data_dir.join(DB_NAME)
    };
    let db_path_str = db_path.to_string_lossy().to_string();
    let base_mods_path_clone = base_mods_path.clone();
    let app_handle_clone = app_handle.clone();
    let maps_clone = deduction_maps.clone(); // Clone maps for the task

    // --- Calculate total expected mods *before* the main walk ---
    println!("[Scan Prep] Calculating total potential mod folders...");
    let potential_mod_folders_for_count: Vec<PathBuf> = WalkDir::new(&base_mods_path)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok().filter(|entry| entry.file_type().is_dir())) // Only consider directories
        .filter(|e| has_ini_file(&e.path().to_path_buf())) // Check if *this* dir contains an ini
        .map(|e| e.path().to_path_buf())
        .collect();

    let total_to_process = potential_mod_folders_for_count.len();
    println!("[Scan Prep] Found {} potential mod folders for progress total.", total_to_process);

    // --- Emit initial progress ---
     app_handle.emit_all(SCAN_PROGRESS_EVENT, ScanProgress {
            processed: 0, total: total_to_process, current_path: None, message: "Starting scan...".to_string()
        }).unwrap_or_else(|e| eprintln!("Failed to emit initial scan progress: {}", e));


    // --- Process folders in a blocking task ---
    let scan_task = async_runtime::spawn_blocking(move || {
        // Open a new connection inside the blocking task
        let conn = Connection::open(&db_path_str).map_err(|e| format!("Failed to open DB connection in scan task: {}", e))?;

        let mut processed_count = 0; // Counts folders *identified* as mods and processed
        let mut mods_added_count = 0;
        let mut mods_updated_count = 0;
        let mut errors_count = 0;
        let mut processed_mod_paths = HashSet::new(); // Track processed paths to avoid duplicates if structure is odd

        // --- Iterate using WalkDir ---
        // We iterate through *all* entries, but only process directories containing .ini
        // `skip_current_dir` will be used *after* processing a mod folder.
        let mut walker = WalkDir::new(&base_mods_path_clone).min_depth(1).into_iter();

        while let Some(entry_result) = walker.next() {
            match entry_result {
                Ok(entry) => {
                    let path = entry.path().to_path_buf();

                    // Check if it's a directory *and* directly contains an ini file *and* not already processed
                    if entry.file_type().is_dir()
                       && has_ini_file(&path)
                       && !processed_mod_paths.contains(&path) // Avoid reprocessing
                    {
                        // *** Found a Mod Folder - Process it ***
                        processed_count += 1; // Increment count of mods processed
                        processed_mod_paths.insert(path.clone()); // Mark as processed
                        let path_display = path.display().to_string();
                        let folder_name_only = path.file_name().unwrap_or_default().to_string_lossy();
                        println!("[Scan Task] Processing identified mod folder #{}: {}", processed_count, path_display);

                        // Emit progress event
                        app_handle_clone.emit_all(SCAN_PROGRESS_EVENT, ScanProgress {
                             processed: processed_count,
                             total: total_to_process, // Use total from pre-calculation
                             current_path: Some(path_display.clone()),
                             message: format!("Processing: {}", folder_name_only)
                         }).unwrap_or_else(|e| eprintln!("Failed to emit scan progress: {}", e));

                        // --- Use new Deduction Logic ---
                        match deduce_mod_info_v2(&path, &base_mods_path_clone, &maps_clone) {
                            Some(deduced) => {
                                 // Use the deduced entity_slug to find the ID
                                 if let Some(target_entity_id) = maps_clone.entity_slug_to_id.get(&deduced.entity_slug) {
                                     // --- Calculate clean relative path correctly ---
                                    let relative_path_buf = match path.strip_prefix(&base_mods_path_clone) {
                                        Ok(p) => p.to_path_buf(),
                                        Err(_) => {
                                            eprintln!("[Scan Task] Error: Could not strip base path prefix from '{}'. Skipping.", path.display());
                                            errors_count += 1;
                                            // No skip_current_dir here, walker continues from next item
                                            continue;
                                        }
                                    };
                                    // Get the filename *from the buffer* which represents the relative path
                                    let filename_osstr = relative_path_buf.file_name().unwrap_or_default();
                                    let filename_str = filename_osstr.to_string_lossy();
                                    let clean_filename = filename_str.trim_start_matches(DISABLED_PREFIX);
                                    let relative_parent_path = relative_path_buf.parent();
                                    let relative_path_to_store = match relative_parent_path {
                                        // Join parent (if exists) with the cleaned filename
                                        Some(parent) => parent.join(clean_filename).to_string_lossy().to_string(),
                                        None => clean_filename.to_string(), // No parent, just the clean filename
                                    };
                                    // Ensure forward slashes for consistency in DB
                                    let relative_path_to_store = relative_path_to_store.replace("\\", "/");
                                    println!("[Scan Task] Storing clean relative path: {}", relative_path_to_store);

                                    // Check if this clean relative path already exists for the entity
                                    let existing_id: Option<i64> = conn.query_row(
                                        "SELECT id FROM assets WHERE entity_id = ?1 AND folder_name = ?2",
                                        params![target_entity_id, relative_path_to_store],
                                        |row| row.get(0),
                                    ).optional().map_err(|e| format!("DB error checking for existing asset '{}': {}", relative_path_to_store, e))?;

                                    if existing_id.is_none() {
                                         // Insert new asset
                                         let insert_result = conn.execute(
                                            "INSERT INTO assets (entity_id, name, description, folder_name, image_filename, author, category_tag) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                                            params![
                                                target_entity_id,
                                                deduced.mod_name, // Use deduced name for display
                                                deduced.description,
                                                relative_path_to_store, // Store the CLEAN relative path
                                                deduced.image_filename,
                                                deduced.author,
                                                deduced.mod_type_tag // Store raw tag from ini
                                            ]
                                         );
                                         match insert_result {
                                             Ok(changes) => { if changes > 0 { mods_added_count += 1; println!("[Scan Task] Added New: {}", relative_path_to_store); } }
                                             Err(e) => { eprintln!("[Scan Task] Error inserting NEW mod from path '{}' with clean relative path '{}': {}", path_display, relative_path_to_store, e); errors_count += 1; }
                                         }
                                     } else {
                                        println!("[Scan Task] Exists (based on clean path): {}", relative_path_to_store);
                                         // Optionally update existing asset data here if needed
                                         // mods_updated_count += 1;
                                    }
                                 } else {
                                      // This case should be less frequent now due to fallback logic
                                      eprintln!("[Scan Task] Error: Deduced entity slug '{}' has no ID in map for mod '{}'. This might indicate an issue with the fallback or maps.", deduced.entity_slug, path.display());
                                      errors_count += 1;
                                 }
                            }
                            None => {
                                 eprintln!("[Scan Task] Error: Could not deduce info for potential mod folder '{}'. Skipping.", path.display());
                                 errors_count += 1;
                            }
                        } // End deduce_mod_info_v2 match

                        // *** CRUCIAL: Tell WalkDir not to descend into this mod folder ***
                        // We've processed it, don't look for mods inside it.
                        println!("[Scan Task] Skipping descent into processed mod folder: {}", path.display());
                        walker.skip_current_dir();

                    } else if entry.file_type().is_dir() {
                        // It's a directory, but NOT identified as a mod folder (no ini or already processed).
                        // Allow WalkDir to continue descending into it implicitly.
                        // println!("[Scan Task] Descending into directory: {}", path.display()); // Debug logging if needed
                    } // else it's a file, WalkDir handles it, just continue.

                } // End Ok(entry)
                Err(e) => {
                     eprintln!("[Scan Task] Error accessing path during scan: {}", e);
                     errors_count += 1;
                }
            } // End match entry_result
        } // End while loop

        // TODO: Add logic here to prune assets from DB that no longer exist on disk? (Separate feature maybe)

        // Return summary info from the blocking task
        Ok::<_, String>((processed_count, mods_added_count, mods_updated_count, errors_count))
    }); // End spawn_blocking

    // --- Handle Task Result (same as before) ---
     match scan_task.await {
         Ok(Ok((processed, added, _updated, errors))) => {
             let summary = format!(
                 "Scan complete. Processed {} identified mod folders. Added {} new mods. {} errors occurred.",
                 processed, added, errors
            );
             println!("{}", summary);
             app_handle.emit_all(SCAN_COMPLETE_EVENT, summary.clone()).unwrap_or_else(|e| eprintln!("Failed to emit scan complete event: {}", e));
             Ok(())
         }
         Ok(Err(e)) => {
             eprintln!("Scan task failed internally: {}", e);
              app_handle.emit_all(SCAN_ERROR_EVENT, e.clone()).unwrap_or_else(|e| eprintln!("Failed to emit scan error event: {}", e));
             Err(e)
         }
         Err(e) => {
             let err_msg = format!("Scan task panicked or failed to join: {}", e);
             eprintln!("{}", err_msg);
             app_handle.emit_all(SCAN_ERROR_EVENT, err_msg.clone()).unwrap_or_else(|e| eprintln!("Failed to emit scan error event: {}", e));
             Err(err_msg)
         }
     }
}

#[command]
fn get_total_asset_count(db_state: State<DbState>) -> CmdResult<i64> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    conn.query_row("SELECT COUNT(*) FROM assets", [], |row| row.get(0))
        .map_err(|e| e.to_string())
}

#[command]
fn update_asset_info(
    asset_id: i64,
    name: String,
    description: Option<String>,
    author: Option<String>,
    category_tag: Option<String>,
    selected_image_absolute_path: Option<String>,
    new_target_entity_slug: Option<String>, // Added for relocation
    db_state: State<DbState>
) -> CmdResult<()> {
    println!("[update_asset_info] Start for asset ID: {}. Relocate to: {:?}", asset_id, new_target_entity_slug);

    let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let conn = &*conn_guard;
    println!("[update_asset_info] DB lock acquired.");

    // --- 1. Get Current Asset Location Info ---
    let current_info = get_asset_location_info(conn, asset_id)
        .map_err(|e| format!("Failed to get current asset info: {}", e))?; // Use internal error type mapping
    println!("[update_asset_info] Current Info: {:?}", current_info);

    // --- 2. Check if Relocation is Requested ---
    // FIX 1: Borrow `current_info.entity_slug`
    let needs_relocation = new_target_entity_slug.is_some() && new_target_entity_slug.as_deref() != Some(&current_info.entity_slug);

    let mut final_entity_id = current_info.entity_id;
    let mut final_relative_path_str = current_info.clean_relative_path.clone();

    if needs_relocation {
        let target_slug = new_target_entity_slug.unwrap(); // Safe unwrap due to check above
        println!("[update_asset_info] Relocation requested to '{}'", target_slug);

        // --- 3a. Get New Category/Entity Info ---
        let (new_entity_id, new_category_slug): (i64, String) = conn.query_row(
            "SELECT e.id, c.slug FROM entities e JOIN categories c ON e.category_id = c.id WHERE e.slug = ?1",
            params![target_slug],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => format!("New target entity '{}' not found.", target_slug),
            _ => format!("DB Error getting new target entity info: {}", e)
        })?;
        println!("[update_asset_info] New target Entity ID: {}, Category Slug: {}", new_entity_id, new_category_slug);

        // --- 3b. Get Base Mods Path ---
        // FIX 2: Map AppError before using `?` on Option
        let base_mods_path_str = get_setting_value(conn, SETTINGS_KEY_MODS_FOLDER)
            .map_err(|e| e.to_string())? // Map AppError -> String
            .ok_or_else(|| "Mods folder path not set".to_string())?;
        let base_mods_path = PathBuf::from(base_mods_path_str);
        println!("[update_asset_info] Base mods path: {}", base_mods_path.display());

        // --- 3c. Determine Current Full Path (Check Enabled/Disabled) ---
        let current_relative_path_buf = PathBuf::from(&current_info.clean_relative_path);
        let current_filename_osstr = current_relative_path_buf.file_name().ok_or_else(|| format!("Could not extract filename from current DB path: {}", current_info.clean_relative_path))?;
        let current_filename_str = current_filename_osstr.to_string_lossy();
        if current_filename_str.is_empty() { return Err("Current filename is empty".to_string()); }
        let disabled_filename = format!("{}{}", DISABLED_PREFIX, current_filename_str);
        let relative_parent_path = current_relative_path_buf.parent();

        let full_path_if_enabled = base_mods_path.join(&current_relative_path_buf);
        let full_path_if_disabled = match relative_parent_path {
           Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
           _ => base_mods_path.join(&disabled_filename),
        };

        let current_full_path = if full_path_if_enabled.is_dir() {
            full_path_if_enabled
        } else if full_path_if_disabled.is_dir() {
            full_path_if_disabled
        } else {
            return Err(format!(
                "Cannot relocate mod '{}': Source folder not found at expected locations derived from DB path '{}' (Checked {} and {}).",
                current_info.id,
                current_info.clean_relative_path,
                full_path_if_enabled.display(),
                full_path_if_disabled.display()
            ));
        };
        println!("[update_asset_info] Current full path on disk: {}", current_full_path.display());


        // --- 3d. Construct New Relative and Full Paths ---
        let mod_base_name = current_filename_str.trim_start_matches(DISABLED_PREFIX); // Use the clean name for the new path
        let new_relative_path_buf = PathBuf::new()
            .join(&new_category_slug)
            .join(&target_slug) // Use the new entity slug provided
            .join(mod_base_name);
        final_relative_path_str = new_relative_path_buf.to_string_lossy().replace("\\", "/"); // Store with forward slashes

        // Construct the new *full* destination path. Respect the original enabled/disabled state by using the base name or prefixed name.
        let new_filename_to_use = if current_full_path.file_name().map_or(false, |name| name.to_string_lossy().starts_with(DISABLED_PREFIX)) {
            disabled_filename // Keep disabled prefix if it was disabled
        } else {
            mod_base_name.to_string() // Use clean name if it was enabled
        };

        let new_full_dest_path = base_mods_path
             .join(&new_category_slug)
             .join(&target_slug)
             .join(&new_filename_to_use); // Use the potentially prefixed name

        println!("[update_asset_info] New relative path for DB: {}", final_relative_path_str);
        println!("[update_asset_info] New full destination path on disk: {}", new_full_dest_path.display());

        // --- 3e. Create Parent Directory for Destination ---
        if let Some(parent) = new_full_dest_path.parent() {
             fs::create_dir_all(parent)
                 .map_err(|e| format!("Failed to create destination parent directory '{}': {}", parent.display(), e))?;
        } else { return Err(format!("Could not determine parent directory for new path: {}", new_full_dest_path.display())); }


        // --- 3f. Perform Filesystem Move ---
        if new_full_dest_path.exists() {
            // This should ideally not happen if mod folder names are unique enough within an entity scope
            // but moving across entities could cause collision. Error out for safety.
             eprintln!("[update_asset_info] Error: Target relocation path already exists: {}", new_full_dest_path.display());
             return Err(format!("Cannot relocate: Target path '{}' already exists.", new_full_dest_path.display()));
        }
        fs::rename(&current_full_path, &new_full_dest_path)
            .map_err(|e| format!("Failed to move mod folder from '{}' to '{}': {}", current_full_path.display(), new_full_dest_path.display(), e))?;
        println!("[update_asset_info] Successfully moved mod folder.");

        // Update final_entity_id for the DB update later
        final_entity_id = new_entity_id;

    } // --- End Relocation Block ---


    // --- 4. Handle Image Copying (Common Logic) ---
    // Get Base Mods Path (if not already fetched during relocation)
    let base_mods_path = if needs_relocation {
         // Already fetched and checked
         PathBuf::from(get_setting_value(conn, SETTINGS_KEY_MODS_FOLDER).map_err(|e|e.to_string())?.ok_or_else(|| "Mods folder path not set".to_string())?)
    } else {
         // FIX 2: Map AppError before using `?` on Option
         PathBuf::from(get_setting_value(conn, SETTINGS_KEY_MODS_FOLDER).map_err(|e|e.to_string())?.ok_or_else(|| "Mods folder path not set".to_string())?)
    };

    // Determine the correct mod folder path *after* potential relocation
    // We use final_relative_path_str which now points to the new location if moved
    let final_mod_folder_path = base_mods_path.join(&final_relative_path_str);
    println!("[update_asset_info] Final mod folder path for image handling: {}", final_mod_folder_path.display());

    // Sanity check: the folder should exist after move/or initially
    // Need to check both potential enabled/disabled states at the *new* location
    let final_filename_osstr = final_mod_folder_path.file_name().ok_or_else(|| format!("Could not extract filename from final path: {}", final_mod_folder_path.display()))?;
    let final_filename_str = final_filename_osstr.to_string_lossy();
    let final_clean_filename = final_filename_str.trim_start_matches(DISABLED_PREFIX);
    let final_disabled_filename = format!("{}{}", DISABLED_PREFIX, final_clean_filename);
    let final_parent_path = final_mod_folder_path.parent().ok_or_else(|| format!("Cannot get parent of final path: {}", final_mod_folder_path.display()))?;

    let final_path_enabled_check = final_parent_path.join(final_clean_filename);
    let final_path_disabled_check = final_parent_path.join(final_disabled_filename);

    let final_path_on_disk = if final_path_enabled_check.is_dir() {
        final_path_enabled_check
    } else if final_path_disabled_check.is_dir() {
        final_path_disabled_check
    } else {
         // If neither exists after the move (or initially if no move), something is wrong
         eprintln!("[update_asset_info] Critical Error: Final mod folder not found on disk after potential move. Checked {} and {}", final_path_enabled_check.display(), final_path_disabled_check.display());
         return Err(format!("Mod folder not found at final destination '{}' after update/move.", final_parent_path.display()));
    };
    println!("[update_asset_info] Confirmed final path on disk for image copy: {}", final_path_on_disk.display());


    let mut image_filename_to_save = current_info.clean_relative_path.split('/').last().map(|s| s.to_string()); // Use existing filename initially

    if let Some(source_path_str) = selected_image_absolute_path {
        println!("[update_asset_info] New image selected: {}", source_path_str);
        let source_path = PathBuf::from(&source_path_str);
        if !source_path.is_file() {
             eprintln!("[update_asset_info] Error: Selected source image file does not exist.");
             return Err(format!("Selected image file does not exist: {}", source_path.display()));
        }

        // Use the confirmed path on disk
        let target_image_path = final_path_on_disk.join(TARGET_IMAGE_FILENAME);
        println!("[update_asset_info] Target image path: {}", target_image_path.display());

        // Ensure parent directory exists (it must if we found final_path_on_disk)
        // fs::create_dir_all(final_path_on_disk.parent().unwrap()) ... // Not needed

        match fs::copy(&source_path, &target_image_path) {
            Ok(_) => {
                println!("[update_asset_info] Image copied successfully.");
                image_filename_to_save = Some(TARGET_IMAGE_FILENAME.to_string());
            }
            Err(e) => {
                eprintln!("[update_asset_info] Failed to copy image: {}", e);
                return Err(format!("Failed to copy image to mod folder: {}", e));
            }
        }
    } else {
         println!("[update_asset_info] No new image selected.");
         // Get existing filename from the current info
         image_filename_to_save = conn.query_row::<Option<String>, _, _>("SELECT image_filename FROM assets WHERE id=?1", params![asset_id], |r|r.get(0)).ok().flatten();
    }
    println!("[update_asset_info] Image handling complete. Filename to save: {:?}", image_filename_to_save);

    // --- 5. Update Database (Common Logic) ---
    println!("[update_asset_info] Attempting DB update for asset ID {} with final_entity_id {} and final_relative_path {}", asset_id, final_entity_id, final_relative_path_str);
    let changes = conn.execute(
        "UPDATE assets SET name = ?1, description = ?2, author = ?3, category_tag = ?4, image_filename = ?5, entity_id = ?6, folder_name = ?7 WHERE id = ?8",
        params![
            name,
            description,
            author,
            category_tag,
            image_filename_to_save,
            final_entity_id,         // Use the potentially updated entity ID
            final_relative_path_str, // Use the potentially updated relative path
            asset_id
        ]
    ).map_err(|e| format!("Failed to update asset info in DB for ID {}: {}", asset_id, e))?;
    println!("[update_asset_info] DB update executed. Changes: {}", changes);

    if changes == 0 {
        eprintln!("[update_asset_info] Warning: DB update affected 0 rows for asset ID {}.", asset_id);
    }

    println!("[update_asset_info] Asset ID {} updated successfully. END", asset_id);
    Ok(())
}

#[command]
fn delete_asset(asset_id: i64, db_state: State<DbState>) -> CmdResult<()> {
     println!("[delete_asset] Attempting to delete asset ID: {}", asset_id);

    let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let conn = &*conn_guard;
    println!("[delete_asset] DB lock acquired.");

    // --- 1. Get Asset Info ---
    let asset_info = get_asset_location_info(conn, asset_id)
        .map_err(|e| format!("Failed to get asset info for deletion: {}", e))?;
    println!("[delete_asset] Asset info found: {:?}", asset_info);

    // --- 2. Get Base Mods Path ---
    let base_mods_path_str = get_setting_value(conn, SETTINGS_KEY_MODS_FOLDER)
        .map_err(|e| format!("Failed to query mods folder setting: {}", e))?
        .ok_or_else(|| "Mods folder path not set".to_string())?;
    let base_mods_path = PathBuf::from(base_mods_path_str);

    // --- 3. Determine Full Path on Disk (Check Enabled/Disabled) ---
     let relative_path_buf = PathBuf::from(&asset_info.clean_relative_path);
     let filename_osstr = relative_path_buf.file_name().ok_or_else(|| format!("Could not extract filename from DB path: {}", asset_info.clean_relative_path))?;
     let filename_str = filename_osstr.to_string_lossy();
     let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
     let relative_parent_path = relative_path_buf.parent();

     let full_path_if_enabled = base_mods_path.join(&relative_path_buf);
     let full_path_if_disabled = match relative_parent_path {
        Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
        _ => base_mods_path.join(&disabled_filename),
     };

    let path_to_delete = if full_path_if_enabled.is_dir() {
        Some(full_path_if_enabled)
    } else if full_path_if_disabled.is_dir() {
        Some(full_path_if_disabled)
    } else {
         // Folder not found, maybe already deleted? Log a warning but proceed to DB deletion.
         eprintln!("[delete_asset] Warning: Mod folder not found on disk for asset ID {}. Checked {} and {}. Proceeding with DB deletion.",
             asset_id, full_path_if_enabled.display(), full_path_if_disabled.display());
         None
    };

    // --- 4. Delete Folder from Filesystem ---
    if let Some(path) = path_to_delete {
         println!("[delete_asset] Deleting folder: {}", path.display());
         fs::remove_dir_all(&path)
            .map_err(|e| format!("Failed to delete mod folder '{}': {}", path.display(), e))?;
         println!("[delete_asset] Folder deleted successfully.");
    }

    // --- 5. Delete from Database ---
    println!("[delete_asset] Deleting asset ID {} from database.", asset_id);
    let changes = conn.execute("DELETE FROM assets WHERE id = ?1", params![asset_id])
        .map_err(|e| format!("Failed to delete asset ID {} from database: {}", asset_id, e))?;

     if changes == 0 {
         // This shouldn't happen if get_asset_location_info succeeded, but good to log.
         eprintln!("[delete_asset] Warning: Database delete affected 0 rows for asset ID {}.", asset_id);
     } else {
         println!("[delete_asset] Database entry deleted successfully.");
     }

    println!("[delete_asset] Asset ID {} deleted successfully. END", asset_id);
    Ok(())
}

#[command]
async fn read_binary_file(path: String) -> Result<Vec<u8>, String> {
    println!("[read_binary_file] Reading path: {}", path);
    // Keep the original path for potential error reporting
    let path_for_error = path.clone(); // Clone the path *before* it's moved

    read_binary(PathBuf::from(path)) // 'path' is moved here
        .map_err(|e| {
            // Use the cloned path 'path_for_error' in the error message
            eprintln!("[read_binary_file] Error reading file '{}': {}", path_for_error, e);
            format!("Failed to read file: {}", e)
        })
}

#[command]
async fn select_archive_file() -> CmdResult<Option<PathBuf>> {
    println!("[select_archive_file] Opening file dialog...");
    let result = dialog::blocking::FileDialogBuilder::new()
        .set_title("Select Mod Archive")
        .add_filter("Archives", &["zip"]) // Start with just zip
        // .add_filter("Archives", &["zip", "rar", "7z"]) // Add others later if needed
        .add_filter("All Files", &["*"])
        .pick_file();

    match result {
        Some(path) => {
            println!("[select_archive_file] File selected: {}", path.display());
            Ok(Some(path))
        },
        None => {
            println!("[select_archive_file] Dialog cancelled.");
            Ok(None)
        }, // User cancelled
    }
}

#[command]
fn analyze_archive(file_path_str: String, db_state: State<DbState>) -> CmdResult<ArchiveAnalysisResult> { // Added db_state (currently unused here, but available)
    println!("[analyze_archive] Analyzing: {}", file_path_str);
    let file_path = PathBuf::from(&file_path_str);
    if !file_path.is_file() {
        return Err(format!("Archive file not found: {}", file_path.display()));
     }

    let file = fs::File::open(&file_path)
        .map_err(|e| format!("Failed to open archive file {}: {}", file_path.display(), e))?;

    let mut archive = ZipArchive::new(file)
        .map_err(|e| format!("Failed to read zip archive {}: {}", file_path.display(), e))?;

    let mut entries = Vec::new();
    let mut ini_contents: HashMap<String, String> = HashMap::new(); // Store path -> content
    let preview_candidates = ["preview.png", "icon.png", "thumbnail.png", "preview.jpg", "icon.jpg", "thumbnail.jpg"];

    // --- Pass 1: Collect entries and read INI files ---
    println!("[analyze_archive] Pass 1: Collecting entries & reading INIs...");
    for i in 0..archive.len() {
        let mut file_entry = match archive.by_index(i) {
            Ok(fe) => fe,
            Err(e) => {
                 println!("[analyze_archive] Warn: Failed read entry #{}: {}", i, e);
                 continue; // Skip this entry if reading fails
            }
        };
        let path_str_opt = file_entry.enclosed_name().map(|p| p.to_string_lossy().replace("\\", "/"));
        if path_str_opt.is_none() {
             println!("[analyze_archive] Warning: Entry #{} has invalid path, skipping.", i);
             continue;
        }
        let path_str = path_str_opt.unwrap();
        let is_dir = file_entry.is_dir();

        // Read content if it's an INI file
        if !is_dir && path_str.to_lowercase().ends_with(".ini") {
            let mut content = String::new();
            if file_entry.read_to_string(&mut content).is_ok() {
                ini_contents.insert(path_str.clone(), content);
            } else {
                 println!("[analyze_archive] Warning: Failed to read content of INI file '{}'", path_str);
            }
        }

        entries.push(ArchiveEntry {
            path: path_str.clone(),
            is_dir,
            is_likely_mod_root: false,
        });
    }
    println!("[analyze_archive] Found {} entries. Found {} INI files.", entries.len(), ini_contents.len());

    // --- Pass 2: Find indices of likely roots (based on INI) ---
    let mut likely_root_indices = HashSet::new();
    println!("[analyze_archive] Pass 2: Finding roots containing INIs...");
    for (ini_index, ini_entry) in entries.iter().enumerate() {
        if !ini_entry.is_dir && ini_entry.path.to_lowercase().ends_with(".ini") {
            // Find its parent directory path within the archive entries
            let parent_path_obj = Path::new(&ini_entry.path).parent();
            if let Some(parent_path_ref) = parent_path_obj {
                 let parent_path_str_norm = parent_path_ref.to_string_lossy().replace("\\", "/");
                 if parent_path_str_norm.is_empty() { continue; } // Skip INI in root

                 // Find the index of the parent directory entry in our list.
                 let found_parent = entries.iter().position(|dir_entry| {
                      if !dir_entry.is_dir { return false; }
                      // Normalize directory entry path (remove trailing slash if present)
                      let dir_entry_path_norm = dir_entry.path.strip_suffix('/').unwrap_or(&dir_entry.path);
                      dir_entry_path_norm == parent_path_str_norm
                 });

                 if let Some(parent_index) = found_parent {
                     println!("[analyze_archive] Found INI '{}' inside potential root '{}' (index {})", ini_entry.path, parent_path_str_norm, parent_index);
                     likely_root_indices.insert(parent_index);
                 } else {
                     println!("[analyze_archive] WARN: Could not find directory entry for parent path '{}' of INI file '{}'", parent_path_str_norm, ini_entry.path);
                 }
            } else {
                  println!("[analyze_archive] WARN: Could not get parent path for INI file '{}'", ini_entry.path);
             }
        }
    }
    println!("[analyze_archive] Identified {} likely root indices: {:?}", likely_root_indices.len(), likely_root_indices);


    // --- Pass 3: Find detected previews inside *potential* roots (Immutable) ---
    println!("[analyze_archive] Pass 3: Checking for preview images in likely roots...");
    let mut root_to_preview_map: HashMap<usize, String> = HashMap::new(); // Map root index -> preview path
    for root_index in likely_root_indices.iter() {
         if let Some(root_entry) = entries.get(*root_index) { // Get immutable ref to root entry
             let root_prefix = if root_entry.path.ends_with('/') { root_entry.path.clone() } else { format!("{}/", root_entry.path) };
             for candidate in preview_candidates.iter() {
                 let potential_preview_path = format!("{}{}", root_prefix, candidate);
                 // Check immutably if this preview exists
                 if entries.iter().any(|e| !e.is_dir && e.path.eq_ignore_ascii_case(&potential_preview_path)) {
                      println!("[analyze_archive] Found potential preview '{}' inside root index {}.", potential_preview_path, root_index);
                     root_to_preview_map.insert(*root_index, potential_preview_path);
                     break; // Found one for this root, move to next root
                 }
             }
         }
    }
     println!("[analyze_archive] Found previews for {} roots.", root_to_preview_map.len());


    // --- Pass 4: Mark roots & attempt deduction (Mutable + DB Access) ---
    println!("[analyze_archive] Pass 4: Marking roots and extracting/deducing info...");
    let mut deduced_mod_name: Option<String> = None;
    let mut deduced_author: Option<String> = None;
    let mut deduced_category_slug: Option<String> = None; // <-- Will try to set this
    let mut deduced_entity_slug: Option<String> = None;   // <-- Will try to set this
    let mut raw_ini_type_found: Option<String> = None;
    let mut raw_ini_target_found: Option<String> = None;
    let mut detected_preview_internal_path : Option<String> = None;
    let mut first_likely_root_processed = false;

    // Acquire lock *once* if we need DB access for deduction
    let conn_guard_opt = if !likely_root_indices.is_empty() {
         Some(db_state.0.lock().map_err(|_| "DB lock poisoned during analysis".to_string())?)
     } else {
         None // No roots found, no need to lock/deduce further
     };
     let conn_opt = conn_guard_opt.as_deref(); // Get Option<&Connection>


    for (index, entry) in entries.iter_mut().enumerate() {
        if likely_root_indices.contains(&index) {
            entry.is_likely_mod_root = true;
             // Only perform deduction using the first likely root encountered
             if !first_likely_root_processed {
                 first_likely_root_processed = true;
                 println!("[analyze_archive] Attempting deduction based on first root: {}", entry.path);
                 let root_prefix = if entry.path.ends_with('/') { entry.path.clone() } else { format!("{}/", entry.path) };

                 // --- Process INI if found ---
                 if let Some((ini_path, ini_content)) = ini_contents.iter().find(|(p, _)| p.starts_with(&root_prefix) && p.trim_start_matches(&root_prefix).find('/') == None) {
                      println!("[analyze_archive] Found INI '{}' inside root for deduction.", ini_path);
                     if let Ok(ini) = Ini::load_from_str(ini_content) {
                        for section_name in ["Mod", "Settings", "Info", "General"] {
                             if let Some(section) = ini.section(Some(section_name)) {
                                 // Deduce Name/Author
                                 let name_val = section.get("Name").or_else(|| section.get("ModName"));
                                 if name_val.is_some() { deduced_mod_name = name_val.map(|s| MOD_NAME_CLEANUP_REGEX.replace_all(s, "").trim().to_string()); }
                                 let author_val = section.get("Author");
                                  if author_val.is_some() { deduced_author = author_val.map(String::from); }

                                 // Extract Raw Type/Target
                                  let target_val = section.get("Target").or_else(|| section.get("Entity")).or_else(|| section.get("Character"));
                                  if target_val.is_some() { raw_ini_target_found = target_val.map(|s| s.trim().to_string()); }
                                  let type_val = section.get("Type").or_else(|| section.get("Category"));
                                  if type_val.is_some() { raw_ini_type_found = type_val.map(|s| s.trim().to_string()); }

                                  // If any relevant field found, break section search
                                 if deduced_mod_name.is_some() || deduced_author.is_some() || raw_ini_target_found.is_some() || raw_ini_type_found.is_some() { break; }
                             }
                         }
                     }
                 } // End INI processing

                 // --- DB Deductions (if lock acquired) ---
                 if let Some(conn) = conn_opt {
                      // 1. Deduce Category Slug
                      if let Some(ref raw_type) = raw_ini_type_found {
                          let lower_raw_type = raw_type.to_lowercase();
                          println!("[analyze_archive] Querying category for raw type: {}", raw_type);
                          let query = "SELECT slug FROM categories WHERE LOWER(slug) = ?1 OR LOWER(name) = ?1 LIMIT 1";
                           match conn.query_row(query, params![lower_raw_type], |row| row.get::<_, String>(0)).optional() {
                               Ok(Some(slug)) => {
                                   println!("[analyze_archive] Deduced category slug: {}", slug);
                                   deduced_category_slug = Some(slug);
                               }
                               Ok(None) => { println!("[analyze_archive] Raw type '{}' not found in categories.", raw_type); }
                               Err(e) => { println!("[analyze_archive] Warn: DB error querying category for type '{}': {}", raw_type, e); } // Log error but continue
                           }
                      }

                      // 2. Deduce Entity Slug (only if target and category found)
                      if let (Some(ref raw_target), Some(ref cat_slug)) = (&raw_ini_target_found, &deduced_category_slug) {
                           let lower_raw_target = raw_target.to_lowercase();
                           println!("[analyze_archive] Querying entity for raw target: {} in category: {}", raw_target, cat_slug);
                            // Query within the specific category first for better accuracy
                           let query = "SELECT e.slug FROM entities e JOIN categories c ON e.category_id = c.id WHERE c.slug = ?1 AND (LOWER(e.slug) = ?2 OR LOWER(e.name) = ?2) LIMIT 1";
                            match conn.query_row(query, params![cat_slug, lower_raw_target], |row| row.get::<_, String>(0)).optional() {
                                Ok(Some(slug)) => {
                                    println!("[analyze_archive] Deduced entity slug: {}", slug);
                                    deduced_entity_slug = Some(slug);
                                }
                                Ok(None) => { println!("[analyze_archive] Raw target '{}' not found in category '{}'.", raw_target, cat_slug); }
                                Err(e) => { println!("[analyze_archive] Warn: DB error querying entity for target '{}': {}", raw_target, e); } // Log error but continue
                            }
                      }
                 } // End DB Deductions

                 // Get the pre-calculated preview path for this root index
                 if let Some(preview_path) = root_to_preview_map.get(&index) {
                      detected_preview_internal_path = Some(preview_path.clone());
                 }
             } // End processing first root
        } // End if root index found
    } // End main mutable loop


     // Fallback name deduction
     if deduced_mod_name.is_none() || deduced_mod_name.as_deref() == Some("") {
         deduced_mod_name = Some(file_path.file_stem().unwrap_or_default().to_string_lossy().to_string());
     }
     // Clean final deduced name
     if let Some(name) = &deduced_mod_name {
          let cleaned = MOD_NAME_CLEANUP_REGEX.replace_all(name, "").trim().to_string();
          if !cleaned.is_empty() { deduced_mod_name = Some(cleaned); }
     }

    println!("[analyze_archive] Final deduction: Name={:?}, Author={:?}, CategorySlug={:?}, EntitySlug={:?}, RawType={:?}, RawTarget={:?}, Preview={:?}",
        deduced_mod_name, deduced_author, deduced_category_slug, deduced_entity_slug, raw_ini_type_found, raw_ini_target_found, detected_preview_internal_path);

    // Lock guard (conn_guard_opt) goes out of scope here if it was acquired

    Ok(ArchiveAnalysisResult {
        file_path: file_path_str,
        entries,
        deduced_mod_name,
        deduced_author,
        deduced_category_slug,
        deduced_entity_slug,
        raw_ini_type: raw_ini_type_found,
        raw_ini_target: raw_ini_target_found,
        detected_preview_internal_path,
    })
}

#[command]
fn read_archive_file_content(archive_path_str: String, internal_file_path: String) -> CmdResult<Vec<u8>> {
    println!("[read_archive_file_content] Reading '{}' from archive '{}'", internal_file_path, archive_path_str);
    let archive_path = PathBuf::from(&archive_path_str);
    if !archive_path.is_file() {
        return Err(format!("Archive file not found: {}", archive_path.display()));
    }

    let file = fs::File::open(&archive_path)
        .map_err(|e| format!("Failed to open archive file {}: {}", archive_path.display(), e))?;

    let mut archive = ZipArchive::new(file)
        .map_err(|e| format!("Failed to read zip archive {}: {}", archive_path.display(), e))?;

    let internal_path_normalized = internal_file_path.replace("\\", "/");

    // --- Apply compiler suggestion: Store result in a variable ---
    let result = match archive.by_name(&internal_path_normalized) {
        Ok(mut file_in_zip) => {
            let mut buffer = Vec::with_capacity(file_in_zip.size() as usize);
            match file_in_zip.read_to_end(&mut buffer) {
                 Ok(_) => {
                     println!("[read_archive_file_content] Successfully read {} bytes.", buffer.len());
                     Ok(buffer) // Ok(Vec<u8>)
                 }
                 Err(e) => {
                      Err(format!("Failed to read internal file content '{}': {}", internal_file_path, e)) // Err(String)
                 }
            }
        },
        Err(zip::result::ZipError::FileNotFound) => {
             Err(format!("Internal file '{}' not found in archive.", internal_file_path)) // Err(String)
        },
        Err(e) => {
             Err(format!("Error accessing internal file '{}': {}", internal_file_path, e)) // Err(String)
        }
    }; // Semicolon here forces the temporary borrow from by_name to end

    result // Return the stored result
}

#[command]
fn import_archive(
    archive_path_str: String,
    target_entity_slug: String,
    selected_internal_root: String,
    mod_name: String,
    description: Option<String>,
    author: Option<String>,
    category_tag: Option<String>,
    selected_preview_absolute_path: Option<String>, // Added
    db_state: State<DbState>
) -> CmdResult<()> {
    println!("[import_archive] Importing '{}', internal path '{}' for entity '{}'", archive_path_str, selected_internal_root, target_entity_slug);
    println!("[import_archive] User provided preview path: {:?}", selected_preview_absolute_path);

     // --- Basic Validation ---
     if mod_name.trim().is_empty() { return Err("Mod Name cannot be empty.".to_string()); }
     if target_entity_slug.trim().is_empty() { return Err("Target Entity must be selected.".to_string()); }
     let archive_path = PathBuf::from(&archive_path_str);
     if !archive_path.is_file() { return Err(format!("Archive file not found: {}", archive_path.display())); }
     println!("[import_archive] Validations passed.");

     // --- Acquire Lock and Get DB Info & Paths ---
     let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
     let conn = &*conn_guard;
     println!("[import_archive] DB lock acquired.");

     // Get Base Mods Path
     let base_mods_path_str = get_setting_value(conn, SETTINGS_KEY_MODS_FOLDER)
         .map_err(|e| format!("Failed to query mods folder setting: {}", e))?
         .ok_or_else(|| "Mods folder path not set".to_string())?;
     let base_mods_path = PathBuf::from(base_mods_path_str);
     println!("[import_archive] Found base mods path: {}", base_mods_path.display());

     // Get Category Slug AND Entity ID
     let (target_category_slug, target_entity_id): (String, i64) = conn.query_row(
         "SELECT c.slug, e.id FROM entities e JOIN categories c ON e.category_id = c.id WHERE e.slug = ?1",
         params![target_entity_slug],
         |row| Ok((row.get(0)?, row.get(1)?)),
     ).map_err(|e| match e {
          rusqlite::Error::QueryReturnedNoRows => format!("Target entity '{}' not found.", target_entity_slug),
          _ => format!("DB Error getting target entity/category info: {}", e)
      })?;
     println!("[import_archive] Found target entity ID: {}, Category Slug: {}", target_entity_id, target_category_slug);

    // Determine target mod folder name
    let target_mod_folder_name = mod_name.trim().replace(" ", "_").replace(".", "_");
    if target_mod_folder_name.is_empty() { return Err("Mod Name results in invalid folder name after cleaning.".to_string()); }
     println!("[import_archive] Target folder name: {}", target_mod_folder_name);

     // Construct the CORRECT final destination path including category
     let final_mod_dest_path = base_mods_path
          .join(&target_category_slug) // Add category slug
          .join(&target_entity_slug)   // Add entity slug
          .join(&target_mod_folder_name); // Add mod folder name

      // Create the full path including category/entity dirs
      fs::create_dir_all(&final_mod_dest_path)
         .map_err(|e| format!("Failed to create destination directory '{}': {}", final_mod_dest_path.display(), e))?;

     println!("[import_archive] Target destination folder created/ensured: {}", final_mod_dest_path.display());

     // --- Extraction Logic (ZIP only) ---
     println!("[import_archive] Opening archive for extraction...");
     let file = fs::File::open(&archive_path)
         .map_err(|e| format!("Failed to open archive file {}: {}", archive_path.display(), e))?;
     let mut archive = ZipArchive::new(file)
         .map_err(|e| format!("Failed to read zip archive {}: {}", archive_path.display(), e))?;

     // Normalize the internal root path
     let prefix_to_extract_norm = selected_internal_root.replace("\\", "/");
     let prefix_to_extract = prefix_to_extract_norm.strip_suffix('/').unwrap_or(&prefix_to_extract_norm);
     let prefix_path = Path::new(prefix_to_extract);
     println!("[import_archive] Normalized internal root prefix: '{}'", prefix_to_extract);

     let mut files_extracted_count = 0;
     for i in 0..archive.len() {
        let mut file_in_zip = archive.by_index(i)
             .map_err(|e| format!("Failed to read entry #{} from zip: {}", i, e))?;

        let internal_path_obj_opt = file_in_zip.enclosed_name().map(|p| p.to_path_buf());
        if internal_path_obj_opt.is_none() { continue; }
        let internal_path_obj = internal_path_obj_opt.unwrap();

        let should_extract = if prefix_to_extract.is_empty() {
             true
         } else {
             internal_path_obj.starts_with(prefix_path)
         };

        if should_extract {
             let relative_path_to_dest = if prefix_to_extract.is_empty() {
                 &internal_path_obj
             } else {
                 match internal_path_obj.strip_prefix(prefix_path) {
                     Ok(p) => p,
                     Err(_) => { continue; } // Skip if prefix stripping fails
                 }
             };

            if relative_path_to_dest.as_os_str().is_empty() { continue; } // Skip root itself

            let outpath = final_mod_dest_path.join(relative_path_to_dest);

            if file_in_zip.is_dir() {
                 fs::create_dir_all(&outpath)
                     .map_err(|e| format!("Failed to create directory '{}': {}", outpath.display(), e))?;
            } else {
                 if let Some(p) = outpath.parent() {
                     if !p.exists() { fs::create_dir_all(&p).map_err(|e| format!("Failed to create parent dir '{}': {}", p.display(), e))?; }
                 }
                 let mut outfile = fs::File::create(&outpath).map_err(|e| format!("Failed to create file '{}': {}", outpath.display(), e))?;
                 std::io::copy(&mut file_in_zip, &mut outfile).map_err(|e| format!("Failed to copy content to '{}': {}", outpath.display(), e))?;
                 files_extracted_count += 1;
            }

             #[cfg(unix)]
             { /* ... set permissions ... */ }
        }
    }
     println!("[import_archive] Extracted {} files.", files_extracted_count);
     if files_extracted_count == 0 && archive.len() > 0 && !selected_internal_root.is_empty() {
          println!("[import_archive] Warning: 0 files extracted. Check if the selected internal root ('{}') was correct.", selected_internal_root);
     }


    // --- Handle Preview Image ---
    let mut image_filename_for_db: Option<String> = None;
    if let Some(user_preview_path_str) = selected_preview_absolute_path {
         let source_path = PathBuf::from(&user_preview_path_str);
          let target_image_path = final_mod_dest_path.join(TARGET_IMAGE_FILENAME);
          println!("[import_archive] Copying user-selected preview '{}' to '{}'", source_path.display(), target_image_path.display());
          if source_path.is_file() {
               fs::copy(&source_path, &target_image_path).map_err(|e| format!("Failed to copy user preview image: {}", e))?;
                image_filename_for_db = Some(TARGET_IMAGE_FILENAME.to_string());
          } else { /* ... warning ... */ }
    } else {
         let potential_extracted_image_path = final_mod_dest_path.join(TARGET_IMAGE_FILENAME);
         if potential_extracted_image_path.is_file() {
              println!("[import_archive] Using extracted {} as preview.", TARGET_IMAGE_FILENAME);
              image_filename_for_db = Some(TARGET_IMAGE_FILENAME.to_string());
         } else { /* ... no preview found log ... */ }
    }


   // --- Add to Database ---
   let relative_path_for_db = Path::new(&target_category_slug)
        .join(&target_entity_slug)
        .join(&target_mod_folder_name);
   let relative_path_for_db_str = relative_path_for_db.to_string_lossy().replace("\\", "/");

   // Check existing
   let check_existing: Option<i64> = conn.query_row(
        "SELECT id FROM assets WHERE entity_id = ?1 AND folder_name = ?2",
        params![target_entity_id, relative_path_for_db_str],
        |row| row.get(0)
   ).optional().map_err(|e| format!("DB error checking for existing imported asset '{}': {}", relative_path_for_db_str, e))?;

    if check_existing.is_some() {
        fs::remove_dir_all(&final_mod_dest_path).ok(); // Attempt cleanup
        return Err(format!("Database entry already exists for '{}'. Aborting.", relative_path_for_db_str));
    }

    // Insert new asset
    println!("[import_archive] Adding asset to DB: entity_id={}, name={}, path={}, image={:?}", target_entity_id, mod_name, relative_path_for_db_str, image_filename_for_db);
    conn.execute(
        "INSERT INTO assets (entity_id, name, description, folder_name, image_filename, author, category_tag) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            target_entity_id, mod_name, description, relative_path_for_db_str,
            image_filename_for_db, author, category_tag
        ]
    ).map_err(|e| {
        fs::remove_dir_all(&final_mod_dest_path).ok(); // Cleanup on DB error
        format!("Failed to add imported mod to database: {}", e)
    })?;

   println!("[import_archive] Import successful for '{}'", mod_name);
   Ok(()) // Lock released here
}

// --- Main Function ---
fn main() {
    let context = generate_context!();

    tauri::Builder::default()
        .setup(|app| {
            let app_handle = app.handle();
             if let Err(e) = initialize_database(&app_handle) {
                 eprintln!("FATAL: Database initialization failed: {}", e);
                 dialog::blocking::message( app_handle.get_window("main").as_ref(), "Fatal Error", format!("Database initialization failed:\n{}", e) );
                 std::process::exit(1);
             }
             println!("Database structure verified/initialized.");
             let data_dir = get_app_data_dir(&app_handle).expect("Failed to get app data dir post-init");
             let db_path = data_dir.join(DB_NAME);
             let conn = Connection::open(&db_path).expect("Failed to open DB for state management");
             app.manage(DbState(Arc::new(Mutex::new(conn))));
             let db_state: State<DbState> = app.state();
             match get_setting_value(&db_state.0.lock().unwrap(), SETTINGS_KEY_MODS_FOLDER) { // Simple unwrap ok in setup
                 Ok(Some(path)) => println!("Mods folder configured to: {}", path),
                 _ => println!("WARN: Mods folder path is not configured yet."),
             }
            Ok(())
        })
        .invoke_handler(generate_handler![
            // Settings
            get_setting, set_setting, select_directory, select_file, launch_executable,
            // Core
            get_categories,
            get_category_entities, // Added
            get_entities_by_category, get_entity_details,
            get_assets_for_entity, toggle_asset_enabled, get_asset_image_path,
            open_mods_folder,
            // Scan & Count
            scan_mods_directory,
            get_total_asset_count,
            // Edit, Import, Delete
            update_asset_info,
            delete_asset, // Added
            read_binary_file,
            select_archive_file,
            analyze_archive,
            import_archive,
            read_archive_file_content,
        ])
        .run(context)
        .expect("error while running tauri application");
}