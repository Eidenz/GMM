// src-tauri/src/main.rs

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use walkdir::WalkDir;
use ini::Ini;
use tauri::PathResolver;
use regex::Regex;
use lazy_static::lazy_static;
use rusqlite::{Connection, OptionalExtension, Result as SqlResult, params, OpenFlags};
use serde::{Serialize, Deserialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufRead, Read, Seek, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Arc};
use tauri::{
    command, generate_context, generate_handler, AppHandle, Manager, State, api::dialog,
    api::process::Command, Window
};
use std::process::exit;
use thiserror::Error;
use once_cell::sync::Lazy;
use tauri::async_runtime;
use toml;
use tauri::api::file::read_binary;
use sevenz_rust::{Password, decompress_file};
use zip::{ZipArchive, result::ZipError};
use unrar::{Archive, Process, List, ListSplit};
use rusqlite::Transaction;
use std::ffi::OsStr;

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

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Preset {
    id: i64,
    name: String,
    is_favorite: bool,
}

#[derive(Clone, serde::Serialize)]
struct ApplyProgress {
  processed: usize,
  total: usize,
  current_asset_id: Option<i64>,
  message: String,
}

#[derive(Serialize, Debug, Clone)]
struct DashboardStats {
    total_mods: i64,
    enabled_mods: i64,
    disabled_mods: i64,
    uncategorized_mods: i64, // Mods in entities ending with "-other"
    category_counts: HashMap<String, i64>, // Category Name -> Count
}

#[derive(Serialize, Debug, Clone)] // Add Serialize
struct KeybindInfo {
    title: String,
    key: String,
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
    #[error("7z error: {0}")]
    SevenZ(#[from] sevenz_rust::Error),
    #[error("RAR error: {0}")]
    Rar(#[from] unrar::error::UnrarError),
    #[error("Unsupported archive type: {0}")]
    UnsupportedArchive(String),
}

// --- Event Payload Struct ---
#[derive(Clone, serde::Serialize)]
struct ScanProgress {
  processed: usize,
  total: usize,
  current_path: Option<String>,
  message: String,
}

const APP_CONFIG_FILENAME: &str = "app_config.json";
const DEFAULT_GAME_SLUG: &str = "genshin";
const PREDEFINED_GAMES: [&str; 3] = ["genshin", "wuwa", "zzz"];
const DB_INTERNAL_GAME_SLUG_KEY: &str = "database_game_slug";
const DB_FILENAME_PREFIX: &str = "app_data_"; // Prefix for archived game dbs
const ACTIVE_DB_FILENAME: &str = "app_data.sqlite";

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AppConfig {
    last_active_game: String,
    requested_active_game: String,
}

// --- Event Names ---
const SCAN_PROGRESS_EVENT: &str = "scan://progress";
const SCAN_COMPLETE_EVENT: &str = "scan://complete";
const SCAN_ERROR_EVENT: &str = "scan://error";
// Add Preset Apply Event Names
const PRESET_APPLY_START_EVENT: &str = "preset://apply_start";
const PRESET_APPLY_PROGRESS_EVENT: &str = "preset://apply_progress";
const PRESET_APPLY_COMPLETE_EVENT: &str = "preset://apply_complete";
const PRESET_APPLY_ERROR_EVENT: &str = "preset://apply_error";

// --- Add Pruning Event ---
const PRUNING_START_EVENT: &str = "prune://start";
const PRUNING_PROGRESS_EVENT: &str = "prune://progress";
const PRUNING_COMPLETE_EVENT: &str = "prune://complete";
const PRUNING_ERROR_EVENT: &str = "prune://error";
// -------------------------

const SETTINGS_KEY_TRAVELER_MIGRATION_COMPLETE: &str = "traveler_migration_complete_v1"; // Added v1 for potential future migrations

type CmdResult<T> = Result<T, String>;

struct DbState(Arc<Mutex<Connection>>);

static DB_CONNECTION: Lazy<Mutex<SqlResult<Connection>>> = Lazy::new(|| {
    Mutex::new(Err(rusqlite::Error::InvalidPath("DB not initialized yet".into())))
});

lazy_static! {
    static ref MOD_NAME_CLEANUP_REGEX: Regex = Regex::new(r"(?i)(_v\d+(\.\d+)*|_DISABLED|DISABLED_|\(disabled\)|^DISABLED_)").unwrap();
    static ref CHARACTER_NAME_REGEX: Regex = Regex::new(r"(?i)(Raiden|Shogun|HuTao|Tao|Zhongli|Ganyu|Ayaka|Kazuha|Yelan|Eula|Klee|Nahida)").unwrap();
    static ref EXCLUDED_INI_FILENAMES: HashSet<String> = {
        let mut set = HashSet::new();
        set.insert("orfix.ini".to_string());
        set.insert("region.ini".to_string());
        set.insert("offset.ini".to_string());
        set.insert("water.ini".to_string());
        set.insert("fixdash.ini".to_string());
        set.insert("deltatime.ini".to_string());
        set.insert("object.ini".to_string());
        set.insert("timer.ini".to_string());
        set
    };
    static ref NAME_CLEANUP_REGEX: Regex = Regex::new(r"(?i)[_\-.\s]+|(_v\d+(\.\d+)*)|(_af)|(_nsfw)|(\(disabled\))|(\(.*\))|(\[.*\])|(^DISABLED_)").unwrap();
    static ref POTENTIAL_NAME_PART_REGEX: Regex = Regex::new(r"^[a-zA-Z\s]+").unwrap();
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

#[derive(Clone)]
struct DeductionMaps {
    category_slug_to_id: HashMap<String, i64>,
    entity_slug_to_id: HashMap<String, i64>,
    lowercase_category_name_to_slug: HashMap<String, String>,
    lowercase_entity_name_to_slug: HashMap<String, String>,
    entity_slug_to_category_slug: HashMap<String, String>,
    lowercase_entity_firstname_to_slug: HashMap<String, String>, // e.g., "ellen" -> "ellen-joe"
    lowercase_entity_first_two_words_to_slug: HashMap<String, String>, // e.g., "ellen joe" -> "ellen-joe"
}

#[derive(Serialize, Deserialize, Debug)] struct Category { id: i64, name: String, slug: String }
#[derive(Serialize, Deserialize, Debug)] struct Entity { id: i64, category_id: i64, name: String, slug: String, description: Option<String>, details: Option<String>, base_image: Option<String>, mod_count: i32, enabled_mod_count: Option<i32>, recent_mod_count: Option<i32>, favorite_mod_count: Option<i32> }
#[derive(Serialize, Deserialize, Debug, Clone)] struct Asset { id: i64, entity_id: i64, name: String, description: Option<String>, folder_name: String, image_filename: Option<String>, author: Option<String>, category_tag: Option<String>, is_enabled: bool }

#[derive(Serialize, Debug, Clone)]
struct EntityWithCounts {
    // Include all fields from Entity that the frontend card needs
    id: i64,
    category_id: i64,
    name: String,
    slug: String,
    details: Option<String>, // JSON string
    base_image: Option<String>,
    // Counts
    total_mods: i64,
    enabled_mods: i64,
}

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

// --- Migration Logic ---
fn run_traveler_migration_logic(
    db_state: &DbState,
    app_handle: &AppHandle, // Keep for path resolution if needed later
) -> Result<String, String> { // Returns success message or error string
    println!("[Migration] Starting Traveler -> Aether/Lumine migration logic...");

    let base_mods_path = get_mods_base_path_from_settings(db_state)
        .map_err(|e| format!("[Migration] Failed to get mods base path: {}", e))?;

    // --- Use a single lock scope for all DB operations ---
    let mut conn_guard = db_state.0.lock().map_err(|_| "[Migration] DB lock poisoned".to_string())?;
    let conn = &mut *conn_guard; // Get mutable access for the transaction

    // --- Check if migration already done ---
    let migration_status = get_setting_value(conn, SETTINGS_KEY_TRAVELER_MIGRATION_COMPLETE)
        .map_err(|e| format!("[Migration] DB Error checking migration status: {}", e))?;
    if migration_status == Some("true".to_string()) {
        let msg = "[Migration] Traveler migration already marked as complete. Skipping.";
        println!("{}", msg);
        return Ok(msg.to_string());
    }

    // --- Get Entity IDs and Category Slugs ---
    let traveler_info: Option<(i64, String)> = conn.query_row(
        "SELECT id, slug FROM entities WHERE slug = 'traveler'", [], |row| Ok((row.get(0)?, row.get(1)?))
    ).optional().map_err(|e| format!("[Migration] DB Error fetching Traveler info: {}", e))?;

    if traveler_info.is_none() {
        let msg = "[Migration] Traveler entity not found. Migration not needed or already partially done.";
        println!("{}", msg);
        // Mark as complete anyway if Traveler doesn't exist
        conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                     params![SETTINGS_KEY_TRAVELER_MIGRATION_COMPLETE, "true"])
            .map_err(|e| format!("[Migration] Failed to mark as complete after Traveler not found: {}", e))?;
        return Ok(msg.to_string());
    }
    let (traveler_id, _traveler_slug) = traveler_info.unwrap(); // Safe due to check above

    // Fetch Aether info (ID, Category Slug)
    let aether_info: Option<(i64, String, String)> = conn.query_row(
        "SELECT e.id, e.slug, c.slug FROM entities e JOIN categories c ON e.category_id = c.id WHERE e.slug = 'aether'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).optional().map_err(|e| format!("[Migration] DB Error fetching Aether info: {}", e))?;

    // Fetch Lumine info (ID, Category Slug)
    let lumine_info: Option<(i64, String, String)> = conn.query_row(
        "SELECT e.id, e.slug, c.slug FROM entities e JOIN categories c ON e.category_id = c.id WHERE e.slug = 'lumine'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).optional().map_err(|e| format!("[Migration] DB Error fetching Lumine info: {}", e))?;

    if aether_info.is_none() || lumine_info.is_none() {
        let msg = "[Migration] Aether or Lumine entity not found. Cannot perform migration. Ensure definitions are loaded.";
        println!("{}", msg);
        // Don't mark as complete, definitions might load later
        return Err(msg.to_string());
    }
    let (aether_id, aether_slug, aether_cat_slug) = aether_info.unwrap();
    let (lumine_id, lumine_slug, lumine_cat_slug) = lumine_info.unwrap();

    // Basic sanity check: Ensure they are in the same category (expected)
    if aether_cat_slug != lumine_cat_slug {
         println!("[Migration] Warning: Aether ({}) and Lumine ({}) appear to be in different categories. Using Aether's category for path construction.", aether_cat_slug, lumine_cat_slug);
         // Proceed using aether_cat_slug as the base category for paths
    }
    let target_category_slug = aether_cat_slug; // Use Aether's (or Lumine's) category slug

    // --- Get Assets associated with Traveler ---
    let mut assets_to_migrate = Vec::<(i64, String, String)>::new(); // (id, name, folder_name)
    { // Scope for statement
        let mut stmt = conn.prepare("SELECT id, name, folder_name FROM assets WHERE entity_id = ?1")
            .map_err(|e| format!("[Migration] Failed to prepare asset fetch statement: {}", e))?;
        let rows = stmt.query_map(
            params![traveler_id],
            |row| Ok((
                row.get(0)?,
                row.get(1)?,
                row.get::<_, String>(2)?
            ))
        )
        .map_err(|e| format!("[Migration] Failed to query Traveler assets: {}", e))?;

        for row_result in rows {
             match row_result {
                 // Note: No change needed here, as `folder` will now correctly be a String
                 Ok((id, name, folder)) => assets_to_migrate.push((id, name, folder.replace("\\", "/"))),
                 Err(e) => return Err(format!("[Migration] Error reading asset row: {}", e)),
             }
        }
    }

    if assets_to_migrate.is_empty() {
        println!("[Migration] No assets found linked to Traveler (ID: {}).", traveler_id);
        // Still need to delete the Traveler entity if it exists
    } else {
        println!("[Migration] Found {} assets to migrate from Traveler.", assets_to_migrate.len());
    }

    // --- Fetch Deduction Maps for Hinting ---
    // Note: We are already inside a lock, so fetch_deduction_maps needs &Connection
    let maps = fetch_deduction_maps(conn)
        .map_err(|e| format!("[Migration] Failed to fetch deduction maps: {}", e))?;


    // --- Start Transaction ---
    let tx = conn.transaction().map_err(|e| format!("[Migration] Failed to start transaction: {}", e))?;

    let mut migrated_count = 0;
    let mut errors: Vec<String> = Vec::new();

    // --- Process each asset ---
    for (asset_id, asset_name, current_clean_relative_path) in assets_to_migrate {
        println!("[Migration] Processing Asset ID: {}, Name: '{}', Current DB Path: '{}'", asset_id, asset_name, current_clean_relative_path);

        // --- Determine Target (Aether/Lumine) ---
        let mut target_id = aether_id; // Default to Aether
        let mut target_slug = aether_slug.clone();
        let mut target_reason = "Default";

        let current_relative_path_buf = PathBuf::from(&current_clean_relative_path);
        let current_folder_name = current_relative_path_buf.file_name().unwrap_or_default().to_string_lossy();

        // Try hinting based on folder name
        if !current_folder_name.is_empty() {
            if let Some(hinted_slug) = find_entity_slug_from_hint(&current_folder_name, &maps) {
                 if hinted_slug == lumine_slug {
                     target_id = lumine_id;
                     target_slug = lumine_slug.clone();
                     target_reason = "Folder name hint";
                 } else if hinted_slug == aether_slug {
                     target_id = aether_id;
                     target_slug = aether_slug.clone();
                     target_reason = "Folder name hint";
                 }
                 // If hint matches something else, ignore it and stick to default Aether
            }
            // Add simple keyword check as fallback?
            else if current_folder_name.to_lowercase().contains("lumine") || current_folder_name.to_lowercase().contains("female") {
                 target_id = lumine_id;
                 target_slug = lumine_slug.clone();
                 target_reason = "Folder name keyword";
            } else if current_folder_name.to_lowercase().contains("aether") || current_folder_name.to_lowercase().contains("male") {
                target_id = aether_id;
                target_slug = aether_slug.clone();
                target_reason = "Folder name keyword";
            }
        }
        println!("[Migration]   -> Assigning to {} (ID: {}) based on: {}", target_slug, target_id, target_reason);

        // --- Calculate Paths ---
        let mod_folder_base_name_from_db = current_relative_path_buf.file_name().unwrap_or_default().to_string_lossy();
        if mod_folder_base_name_from_db.is_empty() {
            let err = format!("[Migration]   -> ERROR: Cannot extract base name from DB path '{}'. Skipping asset {}.", current_clean_relative_path, asset_id);
            println!("{}", err);
            errors.push(err);
            continue;
        }

        // Construct the new *clean* relative path for the DB
        let new_clean_relative_path_buf = PathBuf::new().join(&target_category_slug).join(&target_slug).join(mod_folder_base_name_from_db.as_ref());
        let new_clean_relative_path_str = new_clean_relative_path_buf.to_string_lossy().replace("\\", "/");

        // Determine the current *actual* path on disk (check enabled/disabled)
        let disabled_filename_current = format!("{}{}", DISABLED_PREFIX, mod_folder_base_name_from_db);
        let relative_parent_path_current = current_relative_path_buf.parent();

        let full_path_if_enabled_current = base_mods_path.join(&current_relative_path_buf);
        let full_path_if_disabled_current = match relative_parent_path_current {
            Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename_current),
            _ => base_mods_path.join(&disabled_filename_current),
        };

        let (current_actual_path_on_disk, is_currently_disabled) =
            if full_path_if_enabled_current.is_dir() {
                (full_path_if_enabled_current, false)
            } else if full_path_if_disabled_current.is_dir() {
                (full_path_if_disabled_current, true)
            } else {
                let err = format!("[Migration]   -> ERROR: Source folder not found on disk for asset {} at '{}' or '{}'. Skipping.", asset_id, full_path_if_enabled_current.display(), full_path_if_disabled_current.display());
                println!("{}", err);
                errors.push(err);
                continue; // Skip this asset
            };
        println!("[Migration]   -> Current path on disk: '{}' (Disabled: {})", current_actual_path_on_disk.display(), is_currently_disabled);

        // Construct the new *actual* destination path on disk, preserving disabled state
        let new_folder_name_on_disk = if is_currently_disabled {
            format!("{}{}", DISABLED_PREFIX, mod_folder_base_name_from_db)
        } else {
            mod_folder_base_name_from_db.to_string()
        };
        let new_actual_dest_path_on_disk = base_mods_path.join(&target_category_slug).join(&target_slug).join(&new_folder_name_on_disk);
        println!("[Migration]   -> New destination path on disk: '{}'", new_actual_dest_path_on_disk.display());

        // --- Perform Filesystem Move (before DB commit, but after tx start) ---
        if current_actual_path_on_disk == new_actual_dest_path_on_disk {
             println!("[Migration]   -> Skipping filesystem move (paths are the same). Might indicate only DB update needed.");
        } else {
            // Ensure parent directory exists
            if let Some(parent) = new_actual_dest_path_on_disk.parent() {
                 if !parent.exists() {
                     println!("[Migration]   -> Creating parent directory: {}", parent.display());
                     if let Err(e) = fs::create_dir_all(parent) {
                         let err = format!("[Migration]   -> ERROR: Failed to create parent directory '{}': {}. Skipping asset {}.", parent.display(), e, asset_id);
                         println!("{}", err);
                         errors.push(err);
                         continue; // Skip this asset
                     }
                 }
            } else {
                 let err = format!("[Migration]   -> ERROR: Cannot determine parent directory for new path '{}'. Skipping asset {}.", new_actual_dest_path_on_disk.display(), asset_id);
                 println!("{}", err);
                 errors.push(err);
                 continue; // Skip this asset
            }

            // Check if target exists unexpectedly
            if new_actual_dest_path_on_disk.exists() {
                let err = format!("[Migration]   -> ERROR: Target path '{}' already exists. Skipping asset {}.", new_actual_dest_path_on_disk.display(), asset_id);
                println!("{}", err);
                errors.push(err);
                continue; // Skip this asset
            }

            // Perform the rename
            println!("[Migration]   -> Moving '{}' -> '{}'", current_actual_path_on_disk.display(), new_actual_dest_path_on_disk.display());
            if let Err(e) = fs::rename(&current_actual_path_on_disk, &new_actual_dest_path_on_disk) {
                 let err = format!("[Migration]   -> ERROR: Failed to move folder for asset {}: {}. Skipping.", asset_id, e);
                 println!("{}", err);
                 errors.push(err);
                 continue; // Skip this asset
            }
        }

        // --- Update Database Record (within transaction) ---
        println!("[Migration]   -> Updating DB: asset_id={}, new_entity_id={}, new_folder_name='{}'", asset_id, target_id, new_clean_relative_path_str);
        let changes = tx.execute(
            "UPDATE assets SET entity_id = ?1, folder_name = ?2 WHERE id = ?3",
            params![target_id, new_clean_relative_path_str, asset_id],
        ).map_err(|e| {
            // Don't automatically rollback here, let the main error handling do it
            format!("[Migration]   -> DB Update failed for asset {}: {}", asset_id, e)
        })?; // Propagate error to outer scope

        if changes == 0 {
            println!("[Migration]   -> Warning: DB update affected 0 rows for asset {}.", asset_id);
        }
        migrated_count += 1;

    } // --- End Asset Loop ---

    // --- Delete the Traveler Entity (if migration was successful so far) ---
    if errors.is_empty() {
        println!("[Migration] Deleting Traveler entity (ID: {}) from database.", traveler_id);
        let deleted_entity_count = tx.execute("DELETE FROM entities WHERE id = ?1", params![traveler_id])
           .map_err(|e| format!("[Migration] Failed to delete Traveler entity: {}", e))?;
        if deleted_entity_count > 0 {
            println!("[Migration] Traveler entity successfully deleted.");
        } else {
            println!("[Migration] Traveler entity already deleted or delete failed (0 rows affected).");
        }

        // Mark migration as complete in settings
        println!("[Migration] Marking migration as complete in settings.");
        tx.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                    params![SETTINGS_KEY_TRAVELER_MIGRATION_COMPLETE, "true"])
           .map_err(|e| format!("[Migration] Failed to mark migration as complete: {}", e))?;

        // --- Commit Transaction ---
        tx.commit().map_err(|e| format!("[Migration] Failed to commit transaction: {}", e))?;

        let final_msg = format!("Traveler migration completed successfully. Migrated {} assets.", migrated_count);
        println!("[Migration] {}", final_msg);
        Ok(final_msg)

    } else {
        // --- Rollback Transaction due to errors ---
        let err_summary = format!("[Migration] Migration failed with {} error(s). Rolling back changes.", errors.len());
        println!("{}", err_summary);
        for e in &errors {
            eprintln!("  - {}", e);
        }
        // Rollback happens automatically when `tx` is dropped due to error return
        Err(format!("{}\n{}", err_summary, errors.join("\n")))
    }
}

// --- Helper Functions for Deduction ---

// Function to clean and extract potential base name
fn clean_and_extract_name(input: &str) -> String {
    // First pass: remove specific tags, versions, prefixes, and replace separators with space
    let separators_removed = NAME_CLEANUP_REGEX.replace_all(input, " ");
    // Second pass: Trim whitespace aggressively
    let trimmed = separators_removed.trim();
    // Third pass: Try to isolate the starting name part before numbers or leftover symbols
    if let Some(mat) = POTENTIAL_NAME_PART_REGEX.find(trimmed) {
        mat.as_str().trim().to_lowercase() // Return the matched alphabetic part, trimmed and lowercased
    } else {
        trimmed.to_lowercase() // Fallback to the trimmed version if no clear name part found
    }
}

// Helper function to find entity slug based on a hint string
fn find_entity_slug_from_hint(hint: &str, maps: &DeductionMaps) -> Option<String> {
    if hint.is_empty() { return None; }

    let cleaned_hint = clean_and_extract_name(hint);
    let lower_hint = hint.to_lowercase(); // Original lowercase for exact matches
    println!("[find_entity_slug] Hint: '{}', Cleaned Lower: '{}'", hint, cleaned_hint);

    // --- Matching Strategies ---

    // Priority 1: Exact slug match (original hint, case-sensitive)
    if maps.entity_slug_to_id.contains_key(hint) {
        println!("[find_entity_slug]   -> Match via P1: exact slug.");
        return Some(hint.to_string());
    }
    // Priority 2: Exact lowercase name match (original hint) -> original slug
    if let Some(slug) = maps.lowercase_entity_name_to_slug.get(&lower_hint) {
         println!("[find_entity_slug]   -> Match via P2: exact lowercase name.");
        return Some(slug.clone());
    }
    // Priority 3: Exact *cleaned* hint matches full lowercase name
     if let Some(slug) = maps.lowercase_entity_name_to_slug.get(&cleaned_hint) {
          println!("[find_entity_slug]   -> Match via P3: exact cleaned hint vs full name.");
         return Some(slug.clone());
     }
    // Priority 4: Exact *cleaned* hint matches first two words
     if let Some(slug) = maps.lowercase_entity_first_two_words_to_slug.get(&cleaned_hint) {
         println!("[find_entity_slug]   -> Match via P4: exact cleaned hint vs first two words.");
        return Some(slug.clone());
     }
    // Priority 5: Exact *cleaned* hint matches first name
     if let Some(slug) = maps.lowercase_entity_firstname_to_slug.get(&cleaned_hint) {
          println!("[find_entity_slug]   -> Match via P5: exact cleaned hint vs first name.");
         return Some(slug.clone());
     }

    // *** NEW Priority 6: First word of cleaned hint matches known first name ***
    if let Some(first_word_cleaned) = cleaned_hint.split_whitespace().next() {
        if first_word_cleaned.len() > 1 { // Avoid matching single letters
             if let Some(slug) = maps.lowercase_entity_firstname_to_slug.get(first_word_cleaned) {
                 println!("[find_entity_slug]   -> Match via P6: first word of cleaned hint ('{}') vs first name map.", first_word_cleaned);
                 return Some(slug.clone());
            }
        }
    }

    // Priority 7: Cleaned hint STARTS WITH known full name
    for (entity_name_lower, entity_slug) in &maps.lowercase_entity_name_to_slug {
         // Ensure the known name isn't tiny compared to hint if starts_with is used
         if cleaned_hint.starts_with(entity_name_lower) && entity_name_lower.len() > 2 {
              println!("[find_entity_slug]   -> Match via P7: cleaned hint starts with known full name ('{}').", entity_name_lower);
             return Some(entity_slug.clone());
         }
     }
    // *** NEW Priority 8: Cleaned hint STARTS WITH known first two words ***
    for (entity_name_first_two, entity_slug) in &maps.lowercase_entity_first_two_words_to_slug {
        if cleaned_hint.starts_with(entity_name_first_two) {
            println!("[find_entity_slug]   -> Match via P8: cleaned hint starts with known first two words ('{}').", entity_name_first_two);
            return Some(entity_slug.clone());
        }
    }
    // *** NEW Priority 9: Cleaned hint STARTS WITH known first name ***
    for (entity_name_first, entity_slug) in &maps.lowercase_entity_firstname_to_slug {
        if cleaned_hint.starts_with(entity_name_first) && entity_name_first.len() > 1 { // Avoid matching 'a' etc.
             println!("[find_entity_slug]   -> Match via P9: cleaned hint starts with known first name ('{}').", entity_name_first);
            return Some(entity_slug.clone());
        }
    }


    // Priority 10: Known full name STARTS WITH cleaned hint (less likely useful)
    // for (entity_name_lower, entity_slug) in &maps.lowercase_entity_name_to_slug {
    //     if entity_name_lower.starts_with(&cleaned_hint) && cleaned_hint.len() > 2 {
    //          println!("[find_entity_slug]   -> Match via P10: known name starts with cleaned hint.");
    //         return Some(entity_slug.clone());
    //     }
    // }


    // Priority 11: Known full name CONTAINS cleaned hint (if hint is reasonably long)
     if cleaned_hint.len() > 3 {
         for (entity_name_lower, entity_slug) in &maps.lowercase_entity_name_to_slug {
             if entity_name_lower.contains(&cleaned_hint) {
                  println!("[find_entity_slug]   -> Match via P11: known name contains cleaned hint.");
                 return Some(entity_slug.clone());
             }
         }
     }
     // Priority 12: Known first two words CONTAINS cleaned hint
      if cleaned_hint.len() > 3 {
         for (entity_name_first_two, entity_slug) in &maps.lowercase_entity_first_two_words_to_slug {
             if entity_name_first_two.contains(&cleaned_hint) {
                  println!("[find_entity_slug]   -> Match via P12: known first two words contains cleaned hint.");
                 return Some(entity_slug.clone());
             }
         }
      }
      // Priority 13: Known first name CONTAINS cleaned hint
      if cleaned_hint.len() > 2 { // Can be shorter here maybe
         for (entity_name_first, entity_slug) in &maps.lowercase_entity_firstname_to_slug {
             if entity_name_first.contains(&cleaned_hint) {
                 println!("[find_entity_slug]   -> Match via P13: known first name contains cleaned hint.");
                 return Some(entity_slug.clone());
             }
         }
      }

    println!("[find_entity_slug]   -> No match found.");
    None // No match found
}

fn get_internal_db_slug(db_path: &PathBuf) -> Result<Option<String>, AppError> {
    if !db_path.exists() {
        return Ok(None);
    }
    // Open read-only first to minimize locking issues if possible
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(db_path, flags)
        .or_else(|e| {
             eprintln!("Failed to open DB read-only ({}), trying read-write: {}", db_path.display(), e);
             // Fallback to read-write if read-only fails (e.g., during schema creation?)
             Connection::open(db_path)
        })?;

    // Use the existing helper function
    get_setting_value(&conn, DB_INTERNAL_GAME_SLUG_KEY)
}

fn find_asset_ini_paths(conn: &Connection, asset_id: i64, base_mods_path: &PathBuf) -> Result<Vec<PathBuf>, AppError> {
    println!("[find_asset_ini_paths] CALLED for asset ID: {}", asset_id);
    let asset_info = get_asset_location_info(conn, asset_id)?;

    let relative_path_buf = PathBuf::from(&asset_info.clean_relative_path);
    let filename_osstr = relative_path_buf.file_name().ok_or_else(|| AppError::ModOperation(format!("Could not extract filename from DB path: {}", asset_info.clean_relative_path)))?;
    let filename_str = filename_osstr.to_string_lossy();
    if filename_str.is_empty() {
        println!("[find_asset_ini_paths] ERROR: Filename extracted from DB path is empty: {}", asset_info.clean_relative_path);
        return Err(AppError::ModOperation("Current filename is empty".to_string()));
     }
    let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
    let relative_parent_path = relative_path_buf.parent();

    let full_path_if_enabled = base_mods_path.join(&relative_path_buf);
    let full_path_if_disabled = match relative_parent_path {
        Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
        _ => base_mods_path.join(&disabled_filename),
    };

    let mod_folder_path = if full_path_if_enabled.is_dir() {
        println!("[find_asset_ini_paths] Found enabled path: {}", full_path_if_enabled.display());
        full_path_if_enabled
    } else if full_path_if_disabled.is_dir() {
        println!("[find_asset_ini_paths] Found disabled path: {}", full_path_if_disabled.display());
        full_path_if_disabled
    } else {
        println!("[find_asset_ini_paths] Mod folder not found for asset ID {}. Checked {} and {}", asset_id, full_path_if_enabled.display(), full_path_if_disabled.display());
        return Ok(Vec::new()); // Return empty vec if folder not found
    };

    // --- Collect all .ini files ---
    let mut ini_paths = Vec::new();
    for entry in WalkDir::new(&mod_folder_path).max_depth(1).min_depth(1).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            if let Some(ext) = entry.path().extension() {
                if ext.to_ascii_lowercase() == "ini" {
                    ini_paths.push(entry.path().to_path_buf());
                }
            }
        }
    }
    println!("[find_asset_ini_paths] Found {} INI files in {}: {:?}", ini_paths.len(), mod_folder_path.display(), ini_paths);
    Ok(ini_paths) // Return the collected paths
}

fn fetch_deduction_maps(conn: &Connection) -> SqlResult<DeductionMaps> {
    let mut category_slug_to_id = HashMap::new();
    let mut lowercase_category_name_to_slug = HashMap::new();
    // *** Store category id to slug for the next step ***
    let mut category_id_to_slug = HashMap::<i64, String>::new();
    // ---
    let mut cat_stmt = conn.prepare("SELECT slug, id, name FROM categories")?;
    let cat_rows = cat_stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)))?;
    println!("[fetch_deduction_maps] Processing categories...");
    let mut cat_count = 0;
    for row in cat_rows {
        if let Ok((slug, id, name)) = row {
            lowercase_category_name_to_slug.insert(name.to_lowercase(), slug.clone());
            category_slug_to_id.insert(slug.clone(), id);
            // *** Store ID -> Slug mapping ***
            category_id_to_slug.insert(id, slug);
            // ---
            cat_count += 1;
        }
    }
    println!("[fetch_deduction_maps] Processed {} categories.", cat_count);


    // --- Entity fetching (Modified to get category_id) ---
    let mut entity_slug_to_id = HashMap::new();
    let mut lowercase_entity_name_to_slug = HashMap::new();
    let mut entity_slug_to_category_slug = HashMap::new();
    let mut lowercase_entity_firstname_to_slug = HashMap::new();
    let mut lowercase_entity_first_two_words_to_slug = HashMap::new();
    // ---
    let mut entity_stmt = conn.prepare("SELECT slug, id, name, category_id FROM entities")?;
    let entity_rows = entity_stmt.query_map([], |row| Ok((
        row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?
    )))?;

    println!("[fetch_deduction_maps] Processing entities for advanced lookup...");
    let mut entity_count = 0;
    for row in entity_rows {
        if let Ok((slug, id, name, category_id)) = row {
            entity_slug_to_id.insert(slug.clone(), id);
            let lower_name = name.to_lowercase();
            lowercase_entity_name_to_slug.insert(lower_name.clone(), slug.clone());

            if let Some(cat_slug) = category_id_to_slug.get(&category_id) {
                 entity_slug_to_category_slug.insert(slug.clone(), cat_slug.clone());
            } else { /* log warning */ }

            // *** Populate advanced name maps ***
            let words: Vec<&str> = lower_name.split_whitespace().collect();
            if let Some(first_word) = words.get(0) {
                 // Only add if different from full name to avoid redundancy/collisions
                 if *first_word != lower_name {
                     lowercase_entity_firstname_to_slug.insert(first_word.to_string(), slug.clone());
                 }
            }
            if words.len() >= 2 {
                 let first_two = format!("{} {}", words[0], words[1]);
                 // Only add if different from full name
                 if first_two != lower_name {
                    lowercase_entity_first_two_words_to_slug.insert(first_two, slug.clone());
                 }
            }
            // *** End populating ***

            entity_count += 1;
        } else if let Err(e) = row { /* log error */ }
    }
    println!("[fetch_deduction_maps] Processed {} entities.", entity_count);


    Ok(DeductionMaps {
        category_slug_to_id,
        entity_slug_to_id,
        lowercase_category_name_to_slug,
        lowercase_entity_name_to_slug,
        entity_slug_to_category_slug,
        lowercase_entity_firstname_to_slug,
        lowercase_entity_first_two_words_to_slug,
    })
}

fn deduce_mod_info_v2(
    mod_folder_path: &PathBuf,
    base_mods_path: &PathBuf,
    maps: &DeductionMaps,
) -> Option<DeducedInfo> {
    println!("[Deduce V2 - Entity First] Input Path: {}", mod_folder_path.display());

    let mod_folder_name = match mod_folder_path.file_name() {
         Some(name) => name.to_string_lossy().to_string(),
         None => {
             eprintln!("[Deduce V2] Error: Cannot get folder name from path: {}", mod_folder_path.display());
             return None;
         }
     };

    // --- Initial Info ---
    let mut info = DeducedInfo {
        entity_slug: format!("{}{}", "unknown", OTHER_ENTITY_SUFFIX),
        mod_name: mod_folder_name.clone(),
        mod_type_tag: None, author: None, description: None,
        image_filename: find_preview_image(mod_folder_path),
    };

    let mut found_entity_slug: Option<String> = None;
    let mut ini_target_hint: Option<String> = None;
    let mut ini_type_hint: Option<String> = None;

    // --- 1. Check Parent Folders for ENTITY Match ---
    println!("[Deduce V2] Checking parent folders for ENTITY match...");
    // ... (existing parent folder checking logic using find_entity_slug_from_hint) ...
    let mut current_path = mod_folder_path.parent();
    while let Some(path) = current_path {
        if path == *base_mods_path || path.parent() == Some(base_mods_path) { break; }
        if let Some(folder_name) = path.file_name().and_then(|n| n.to_str()) {
             if let Some(slug) = find_entity_slug_from_hint(folder_name, maps) {
                 found_entity_slug = Some(slug);
                 println!("[Deduce V2]   -> Found entity via parent folder: '{}' -> {}", folder_name, found_entity_slug.as_ref().unwrap());
                 break;
             }
        }
        current_path = path.parent();
    }
    println!("[Deduce V2] Parent folder check done. Found Entity Slug: {:?}", found_entity_slug);


    // --- 2. Parse INI File (if entity not found yet or for metadata) ---
    println!("[Deduce V2] Checking INI file...");
    let ini_path_option = WalkDir::new(mod_folder_path)
        .max_depth(1).min_depth(1).into_iter()
        .filter_map(|e| e.ok())
        .find(|entry| entry.file_type().is_file() && entry.path().extension().map_or(false, |ext| ext.eq_ignore_ascii_case("ini")))
        .map(|e| e.into_path());

    if let Some(ini_path) = ini_path_option {
        println!("[Deduce V2] Found INI: {}", ini_path.display());
        if let Ok(ini_content) = fs::read_to_string(&ini_path) {
            if let Ok(ini) = Ini::load_from_str(&ini_content) {
                 for section_name in ["Mod", "Settings", "Info", "General"] {
                    if let Some(section) = ini.section(Some(section_name)) {
                        // Update metadata if found
                        if let Some(name) = section.get("Name").or_else(|| section.get("ModName")) { info.mod_name = name.trim().to_string(); }
                        if let Some(author) = section.get("Author") { info.author = Some(author.trim().to_string()); }
                        if let Some(desc) = section.get("Description") { info.description = Some(desc.trim().to_string()); }
                        // Get hints (even if entity found, these might be useful someday)
                        if let Some(target) = section.get("Target").or_else(|| section.get("Entity")).or_else(|| section.get("Character")) { ini_target_hint = Some(target.trim().to_string()); }
                        if let Some(typ) = section.get("Type").or_else(|| section.get("Category")) { info.mod_type_tag = Some(typ.trim().to_string()); ini_type_hint = info.mod_type_tag.clone(); } // Store type hint
                    }
                }
                println!("[Deduce V2] INI parsed. Name='{}', Author='{:?}', TargetHint='{:?}', TypeHint='{:?}'", info.mod_name, info.author, ini_target_hint, ini_type_hint);
            } else {
                eprintln!("[Deduce V2] Warning: Failed to parse INI content from {}", ini_path.display());
            }
        } else {
             eprintln!("[Deduce V2] Warning: Failed to read INI file content from {}", ini_path.display());
        }
    } else {
        println!("[Deduce V2] No INI file found in mod folder.");
    }

    // --- 3. Try Matching INI Target Hint (if entity still not found) ---
    if found_entity_slug.is_none() {
        if let Some(target_hint) = &ini_target_hint {
            println!("[Deduce V2] Trying INI target hint matching...");
            if let Some(slug) = find_entity_slug_from_hint(target_hint, maps) {
                 found_entity_slug = Some(slug);
                 println!("[Deduce V2]   -> Found entity via INI target hint: '{}' -> {}", target_hint, found_entity_slug.as_ref().unwrap());
            }
        }
    }

    // --- 4. Try Matching Internal Filenames (NEW STEP) ---
    if found_entity_slug.is_none() {
        println!("[Deduce V2] Trying internal filename matching...");
        let mut file_match_found = false;
        // Iterate through files directly inside the mod folder (depth 1)
        for entry_result in WalkDir::new(mod_folder_path).min_depth(1).max_depth(1).into_iter() {
             match entry_result {
                 Ok(entry) => {
                     if entry.file_type().is_file() {
                         // Get filename stem (without extension)
                         if let Some(stem) = entry.path().file_stem().and_then(OsStr::to_str) {
                             if !stem.is_empty() {
                                 // Use the helper to check if the stem matches an entity
                                 if let Some(slug) = find_entity_slug_from_hint(stem, maps) {
                                     found_entity_slug = Some(slug);
                                     println!("[Deduce V2]   -> Found entity via internal filename stem: '{}' -> {}", stem, found_entity_slug.as_ref().unwrap());
                                     file_match_found = true;
                                     break; // Found a match from a file, stop searching files
                                 }
                             }
                         }
                     }
                 },
                 Err(e) => {
                     eprintln!("[Deduce V2] Warning: Error accessing entry during internal file scan: {}", e);
                     // Continue scanning other files if possible
                 }
             }
        }
        if !file_match_found {
            println!("[Deduce V2]   -> No entity match found from internal filenames.");
        }
    }


    // --- 5. Try Matching Mod Folder Name (Lower Priority) ---
     if found_entity_slug.is_none() {
         println!("[Deduce V2] Trying mod folder name matching: '{}'", mod_folder_name);
         if let Some(slug) = find_entity_slug_from_hint(&mod_folder_name, maps) {
              found_entity_slug = Some(slug);
              println!("[Deduce V2]   -> Found entity via mod folder name: '{}' -> {}", mod_folder_name, found_entity_slug.as_ref().unwrap());
         }
     }

    // --- 6. Final Assignment Logic ---
    println!("[Deduce V2] Final Assignment Logic. Found Entity Slug So Far: {:?}", found_entity_slug);
    if let Some(ref entity_slug) = found_entity_slug {
        // ---- ENTITY FOUND ----
        // Assign the specific entity slug directly.
        // The category is implicitly determined by this entity's relationship in the DB.
        info.entity_slug = entity_slug.clone();
        println!("[Deduce V2] SUCCESS: Assigning specific entity slug: {}", info.entity_slug);

    } else {
        // ---- ENTITY NOT FOUND ----
        // Fallback: Try to find the most likely CATEGORY to place this mod under,
        //           using the "<category-slug>-other" pattern.
        println!("[Deduce V2] Entity not found. Trying CATEGORY fallback deduction...");
        let mut fallback_category_slug: Option<String> = None;

        // Fallback Priority 1: Parent folder names matching a CATEGORY name/slug
        println!("[Deduce V2]   Fallback Prio 1: Checking parent folders for CATEGORY match...");
        let mut current_path_cat = mod_folder_path.parent();
        while let Some(path) = current_path_cat {
             // Stop if we reach the base mods path or its immediate parent
             if path == *base_mods_path || path.parent() == Some(base_mods_path) { break; }
             if let Some(folder_name) = path.file_name().and_then(|n| n.to_str()) {
                 let lower_folder_name = folder_name.to_lowercase();
                 println!("[Deduce V2]     Checking parent folder for category: {}", folder_name);
                  // Check Category Slug (exact match)
                  if maps.category_slug_to_id.contains_key(folder_name) {
                      fallback_category_slug = Some(folder_name.to_string());
                      println!("[Deduce V2]       -> Found category via parent exact slug: {}", folder_name);
                      break; // Found best match, stop walking up
                  }
                  // Check Lowercase Category Name
                  if let Some(slug) = maps.lowercase_category_name_to_slug.get(&lower_folder_name) {
                      fallback_category_slug = Some(slug.clone());
                      println!("[Deduce V2]       -> Found category via parent lowercase name: {} -> {}", lower_folder_name, slug);
                      break; // Found best match, stop walking up
                  }
             }
             current_path_cat = path.parent();
        }

        // Fallback Priority 2: INI Type Hint matching a CATEGORY name/slug
        if fallback_category_slug.is_none() {
            println!("[Deduce V2]   Fallback Prio 2: Checking INI type hint for CATEGORY match...");
            if let Some(type_hint) = &ini_type_hint {
                let lower_type_hint = type_hint.to_lowercase();
                println!("[Deduce V2]     Trying INI type hint: '{}'", type_hint);
                // Prio 1: Exact slug
                if maps.category_slug_to_id.contains_key(type_hint) {
                    fallback_category_slug = Some(type_hint.clone());
                    println!("[Deduce V2]       -> Matched category via INI exact slug: {}", type_hint);
                }
                // Prio 2: Exact lowercase name -> original slug
                else if let Some(slug) = maps.lowercase_category_name_to_slug.get(&lower_type_hint) {
                    fallback_category_slug = Some(slug.clone());
                    println!("[Deduce V2]       -> Matched category via INI exact lowercase name: {} -> {}", lower_type_hint, slug);
                }
                 // Prio 3: Known name starts with hint (optional)
                 else {
                     for (cat_name_lower, cat_slug) in &maps.lowercase_category_name_to_slug {
                         if cat_name_lower.starts_with(&lower_type_hint) {
                             fallback_category_slug = Some(cat_slug.clone());
                             println!("[Deduce V2]       -> Matched category via INI name prefix: {} -> {}", cat_name_lower, cat_slug);
                             break;
                         }
                     }
                 }
                 // Prio 4: Known name contains hint (optional)
                 if fallback_category_slug.is_none() {
                     for (cat_name_lower, cat_slug) in &maps.lowercase_category_name_to_slug {
                         if lower_type_hint.len() > 2 && cat_name_lower.contains(&lower_type_hint) {
                             fallback_category_slug = Some(cat_slug.clone());
                             println!("[Deduce V2]       -> Matched category via INI name contains: {} -> {}", cat_name_lower, cat_slug);
                             break;
                         }
                     }
                 }
            } else {
                 println!("[Deduce V2]     No INI type hint available.");
            }
        }

        // Fallback Priority 3: Top-level folder name (relative to base) matching a CATEGORY name/slug
        if fallback_category_slug.is_none() {
             println!("[Deduce V2]   Fallback Prio 3: Trying top-level folder name for CATEGORY match...");
             let relative_path_result = mod_folder_path.strip_prefix(base_mods_path);
             if let Ok(relative_path) = relative_path_result {
                 if let Some(top_level_component) = relative_path.components().next() {
                     if let Some(top_folder_name) = top_level_component.as_os_str().to_str() {
                         let lower_top_folder = top_folder_name.to_lowercase();
                         println!("[Deduce V2]     Top-level folder: {}", top_folder_name);
                          // Fuzzy match logic (simplified: check starts_with or contains)
                         // Prioritize exact slug/name match if possible within top-level check
                         if maps.category_slug_to_id.contains_key(top_folder_name) {
                              fallback_category_slug = Some(top_folder_name.to_string());
                              println!("[Deduce V2]       -> Matched category via top-level exact slug: {}", top_folder_name);
                         } else if let Some(slug) = maps.lowercase_category_name_to_slug.get(&lower_top_folder) {
                              fallback_category_slug = Some(slug.clone());
                              println!("[Deduce V2]       -> Matched category via top-level exact name: {} -> {}", lower_top_folder, slug);
                         } else {
                             // Then try fuzzy matching
                             let mut fuzzy_match_found = false;
                             for (cat_slug, _) in &maps.category_slug_to_id {
                                 if cat_slug.starts_with(&lower_top_folder) || lower_top_folder.starts_with(cat_slug) {
                                     fallback_category_slug = Some(cat_slug.clone());
                                     println!("[Deduce V2]       -> Matched category via top-level fuzzy slug prefix: {}", cat_slug);
                                     fuzzy_match_found = true;
                                     break;
                                 }
                             }
                             if !fuzzy_match_found {
                                 for (cat_name_lower, cat_slug) in &maps.lowercase_category_name_to_slug {
                                     if cat_name_lower.starts_with(&lower_top_folder) || lower_top_folder.starts_with(cat_name_lower) {
                                         fallback_category_slug = Some(cat_slug.clone());
                                         println!("[Deduce V2]       -> Matched category via top-level fuzzy name prefix: {} -> {}", cat_name_lower, cat_slug);
                                         fuzzy_match_found = true;
                                         break;
                                     }
                                 }
                             }
                             // Add 'contains' as last resort for fuzzy match
                             if !fuzzy_match_found {
                                 for (cat_name_lower, cat_slug) in &maps.lowercase_category_name_to_slug {
                                     if lower_top_folder.len() > 2 && cat_name_lower.contains(&lower_top_folder) {
                                          fallback_category_slug = Some(cat_slug.clone());
                                          println!("[Deduce V2]       -> Matched category via top-level fuzzy name contains: {} -> {}", cat_name_lower, cat_slug);
                                          break;
                                     }
                                 }
                             }
                         }
                     } else { println!("[Deduce V2]     Could not convert top-level OsStr to str."); }
                 } else { println!("[Deduce V2]     Could not get top-level component."); }
             } else { println!("[Deduce V2]     Could not strip base path prefix."); }
        }

        // --- Assign final fallback slug ---
        if let Some(cat_slug) = fallback_category_slug {
             // Found a category hint, assign to its -other group
             info.entity_slug = format!("{}{}", cat_slug, OTHER_ENTITY_SUFFIX);
             println!("[Deduce V2] Assigning fallback category slug: {}", info.entity_slug);
        } else {
             // Absolute last resort: Use a default category's -other group
             let hardcoded_fallback_category = "characters"; // Or use "unknown" if you have an unknown-other slug
             info.entity_slug = format!("{}{}", hardcoded_fallback_category, OTHER_ENTITY_SUFFIX);
             println!("[Deduce V2] No category hint found, assigning hardcoded fallback: {}", info.entity_slug);
        }
    } // --- End of else block (Entity Not Found) ---

    // --- 7. Clean up Mod Name ---
    let original_mod_name = info.mod_name.clone();
    info.mod_name = MOD_NAME_CLEANUP_REGEX.replace_all(&info.mod_name, "").trim().to_string();
    // If cleaning results in empty, use original folder name as fallback
    if info.mod_name.is_empty() {
         info.mod_name = mod_folder_name;
         println!("[Deduce V2] Warning: Name cleanup resulted in empty string, using folder name '{}'", info.mod_name);
    } else if info.mod_name != original_mod_name {
        println!("[Deduce V2] Cleaned mod name: '{}' -> '{}'", original_mod_name, info.mod_name);
    }

    println!("[Deduce V2 - Entity First] Final Deduced Info: {:?}", info);
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

    let mut has_any_ini = false;
    let mut has_non_excluded_ini = false;

    // Use walkdir limited to depth 1
    for entry_result in WalkDir::new(dir_path).max_depth(1).min_depth(1).into_iter() {
        match entry_result {
            Ok(entry) => {
                if entry.file_type().is_file() {
                    if let Some(ext) = entry.path().extension() {
                        if ext.eq_ignore_ascii_case("ini") {
                            has_any_ini = true; // Found at least one INI file

                            // Get the filename, convert to lowercase
                            if let Some(filename_osstr) = entry.path().file_name() {
                                let filename_lower = filename_osstr.to_string_lossy().to_lowercase();

                                // Check if it's an excluded file (considering DISABLED_ prefix)
                                let base_filename = if filename_lower.starts_with(DISABLED_PREFIX.to_lowercase().as_str()) {
                                    filename_lower.trim_start_matches(DISABLED_PREFIX.to_lowercase().as_str())
                                } else {
                                    filename_lower.as_str()
                                };

                                if !EXCLUDED_INI_FILENAMES.contains(base_filename) {
                                    // Found an INI file that is NOT excluded
                                    has_non_excluded_ini = true;
                                    // Optimization: We can stop searching as soon as we find one non-excluded INI
                                    return true;
                                }
                                // If it IS excluded, we continue searching other files in the directory
                            }
                        }
                    }
                }
            },
            Err(e) => {
                // Log error accessing entry but continue scan if possible
                eprintln!("[has_ini_file] Error accessing entry in {}: {}", dir_path.display(), e);
            }
        }
    }

    // If the loop finishes, return true only if a non-excluded INI was found.
    // This implicitly handles the case where only excluded INIs were found (has_any_ini=true, has_non_excluded_ini=false -> returns false)
    // and the case where no INIs were found (has_any_ini=false, has_non_excluded_ini=false -> returns false).
    has_non_excluded_ini
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

fn get_app_config_path(app_handle: &AppHandle) -> Result<PathBuf, AppError> {
    get_app_data_dir(app_handle).map(|dir| dir.join(APP_CONFIG_FILENAME))
}

fn read_app_config(app_handle: &AppHandle) -> Result<AppConfig, AppError> {
    let config_path = get_app_config_path(app_handle)?;
    if !config_path.exists() {
        println!("App config not found, creating default.");
        // Default has last and requested as the same initially
        let default_config = AppConfig {
            last_active_game: DEFAULT_GAME_SLUG.to_string(),
            requested_active_game: DEFAULT_GAME_SLUG.to_string(),
        };
        write_app_config(app_handle, &default_config)?;
        return Ok(default_config);
    }

    let config_content = fs::read_to_string(&config_path)?;
    serde_json::from_str(&config_content).map_err(AppError::Json)
}

fn write_app_config(app_handle: &AppHandle, config: &AppConfig) -> Result<(), AppError> {
    let config_path = get_app_config_path(app_handle)?;
    let config_content = serde_json::to_string_pretty(config)?;
    fs::write(&config_path, config_content).map_err(AppError::Io)
}

// --- Helper Function to get current enabled state (reusable) ---
fn get_current_asset_enabled_state(conn: &Connection, asset_id: i64, base_mods_path: &PathBuf) -> Result<bool, AppError> {
    let asset_info = get_asset_location_info(conn, asset_id)?; // Reuse existing helper

    let relative_path_buf = PathBuf::from(&asset_info.clean_relative_path);
    let filename_osstr = relative_path_buf.file_name()
        .ok_or_else(|| AppError::ModOperation(format!("Could not extract filename from DB path: {}", asset_info.clean_relative_path)))?;
    let filename_str = filename_osstr.to_string_lossy();
    if filename_str.is_empty() {
        return Err(AppError::ModOperation("Current filename is empty".to_string()));
    }

    // Check ONLY the enabled path based on the CLEAN relative path from DB
    let full_path_if_enabled = base_mods_path.join(&relative_path_buf);

    Ok(full_path_if_enabled.is_dir()) // Return true if the 'enabled' path exists
}

// --- Database Initialization (Result type uses AppError internally) ---
fn initialize_database(app_handle: &AppHandle, active_game_slug: &str) -> Result<Connection, AppError> {
    let data_dir = get_app_data_dir(app_handle)?;
    let db_path = data_dir.join(ACTIVE_DB_FILENAME);
    println!("Initializing database for game '{}' at: {}", active_game_slug, db_path.display());
    let needs_schema_setup = !db_path.exists();

    let conn = Connection::open(&db_path)?;
    conn.execute("PRAGMA foreign_keys = ON;", [])?;

    if needs_schema_setup {
        println!("Performing initial schema setup for {}", db_path.display());
        // --- Create Tables (Same as before) ---
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE categories ( id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE NOT NULL, slug TEXT UNIQUE NOT NULL );
             CREATE TABLE entities ( id INTEGER PRIMARY KEY AUTOINCREMENT, category_id INTEGER NOT NULL, name TEXT NOT NULL, slug TEXT UNIQUE NOT NULL, description TEXT, details TEXT, base_image TEXT, FOREIGN KEY (category_id) REFERENCES categories (id) ON DELETE CASCADE );
             CREATE TABLE assets ( id INTEGER PRIMARY KEY AUTOINCREMENT, entity_id INTEGER NOT NULL, name TEXT NOT NULL, description TEXT, folder_name TEXT NOT NULL UNIQUE, image_filename TEXT, author TEXT, category_tag TEXT, FOREIGN KEY (entity_id) REFERENCES entities (id) ON DELETE CASCADE );
             CREATE TABLE settings ( key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL );
             CREATE TABLE presets ( id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE NOT NULL, is_favorite INTEGER NOT NULL DEFAULT 0 );
             CREATE TABLE preset_assets ( preset_id INTEGER NOT NULL, asset_id INTEGER NOT NULL, is_enabled INTEGER NOT NULL, PRIMARY KEY (preset_id, asset_id), FOREIGN KEY (preset_id) REFERENCES presets(id) ON DELETE CASCADE, FOREIGN KEY (asset_id) REFERENCES assets(id) ON DELETE CASCADE );
             COMMIT;",
        )?;
        println!("Database tables created for {}.", db_path.display());
        println!("Storing internal game slug '{}' in the new database.", active_game_slug);
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![DB_INTERNAL_GAME_SLUG_KEY, active_game_slug],
        )?;

    } else {
        println!("Database file {} already exists.", db_path.display());
        // Optional: Verify internal slug matches expected active_game_slug?
        match get_internal_db_slug(&db_path) {
            Ok(Some(internal_slug)) if internal_slug != active_game_slug => {
                 eprintln!("WARNING: Existing database {} contains slug '{}' but expected '{}'. Check startup logic.", db_path.display(), internal_slug, active_game_slug);
                 // We proceed, assuming the startup logic handled the rename correctly.
            },
            Err(e) => eprintln!("Warning: Could not read internal slug from existing DB {}: {}", db_path.display(), e),
            _ => {} // Slug matches or doesn't exist (old DB?)
        }
    }

    // --- Load Definitions ---
    let definition_resource_path = format!("definitions/{}.toml", active_game_slug);
    println!("Attempting to load definitions from resource: {}", definition_resource_path);

    let definitions: Definitions = match app_handle.path_resolver().resolve_resource(&definition_resource_path) {
        Some(path) => {
            println!("Found definition file at: {}", path.display());
            match fs::read_to_string(&path) {
                Ok(toml_content) => {
                    match toml::from_str(&toml_content) {
                        Ok(defs) => {
                            println!("Successfully parsed definitions for '{}'.", active_game_slug);
                            defs
                        },
                        Err(e) => {
                            eprintln!("ERROR: Failed to parse TOML from {}: {}. Using empty definitions.", path.display(), e);
                            HashMap::new() // Use empty definitions on parse error
                        }
                    }
                },
                Err(e) => {
                    eprintln!("ERROR: Failed to read definition file {}: {}. Using empty definitions.", path.display(), e);
                    HashMap::new() // Use empty definitions on read error
                }
            }
        },
        None => {
            eprintln!("ERROR: Definition file resource '{}' not found. Using empty definitions.", definition_resource_path);
            HashMap::new() // Use empty definitions if file not found
        }
    };

    println!("Loaded {} categories from definitions for '{}'.", definitions.len(), active_game_slug);

    // --- Populate DB from loaded definitions (Same logic as before) ---
    if !definitions.is_empty() {
         for (category_slug, category_def) in definitions.iter() {
             // Wrap inserts in transaction for potential rollback if needed later
             conn.execute( "INSERT OR IGNORE INTO categories (name, slug) VALUES (?1, ?2)", params![category_def.name, category_slug],)?;
             let category_id: i64 = conn.query_row( "SELECT id FROM categories WHERE slug = ?1", params![category_slug], |row| row.get(0), )?;

             let other_slug = format!("{}{}", category_slug, OTHER_ENTITY_SUFFIX);
             conn.execute( "INSERT OR IGNORE INTO entities (category_id, name, slug, description, details, base_image) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![ category_id, OTHER_ENTITY_NAME, other_slug, "Uncategorized assets.", "{}", None::<String> ] )?;

             for entity_def in category_def.entities.iter() {
                 conn.execute( "INSERT OR IGNORE INTO entities (category_id, name, slug, description, details, base_image) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![ category_id, entity_def.name, entity_def.slug, entity_def.description, entity_def.details.as_ref().map(|s| s.to_string()).unwrap_or("{}".to_string()), entity_def.base_image, ] )?;
             }
         }
         println!("Populated database with definitions for '{}'.", active_game_slug);
    } else {
         println!("Skipping definition population as no definitions were loaded for '{}'.", active_game_slug);
    }
    // --- End Definition Population ---
    
    Ok(conn)
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

#[cfg(target_os = "windows")]
#[command]
fn launch_executable_elevated(path: String) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    println!("Attempting elevated launch for: {}", path);

    // Convert the path and verb to Windows wide strings (UTF-16)
    let path_wide: Vec<u16> = std::ffi::OsStr::new(&path)
        .encode_wide()
        .chain(std::iter::once(0)) // Null-terminate
        .collect();
    let operation_wide: Vec<u16> = std::ffi::OsStr::new("runas") // "runas" verb requests elevation
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Call ShellExecuteW to request elevation
    // HINSTANCE returned is actually an integer value; > 32 indicates success.
    let result = unsafe {
        ShellExecuteW(
            Some(HWND::default()),               // Pass as Option<HWND>
            PCWSTR(operation_wide.as_ptr()),     // Operation: "runas"
            PCWSTR(path_wide.as_ptr()),          // File path
            None,                                // No parameters
            None,                                // Default working directory
            SW_SHOWNORMAL,                       // Show command normal state
        )
    };

    // --- FIX 1: Explicitly cast result.0 to isize BEFORE comparison ---
    let result_value = result.0 as isize;

    if result_value > 32 { // Compare the casted value
        println!("Elevated launch initiated successfully via ShellExecuteW.");
        Ok(())
    } else {
        // result.0 contains the error code as an isize, cast to i32 if needed elsewhere
        let error_code = result_value as i32; // Cast for error reporting
        let error_message = format!(
            "Failed to request elevated launch for '{}'. ShellExecuteW error code: {}",
            path, error_code
        );
        eprintln!("{}", error_message);

        // --- FIX 2: Cast the ERROR_CANCELLED constant to i32 for comparison ---
        if error_code == windows::Win32::Foundation::ERROR_CANCELLED.0 as i32 {
             Err("Operation cancelled by user.".to_string())
        } else {
             Err(error_message)
        }
    }
}

#[command]
async fn launch_executable(path: String, _app_handle: AppHandle) -> CmdResult<()> { // app_handle might not be needed now
    println!("Attempting to launch (non-elevated) via Command::new: {}", path);

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
                         // If we get the elevation error here, we could suggest the elevated launch
                         if e.contains("os error 740") {
                             return Err(format!("Failed to launch: The application requires administrator privileges. Try the 'Launch as Admin' button if available, or run GMM as administrator (not recommended). Original error: {}", e));
                         }
                         // Decide if other errors constitute a failure
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
                    _ => {} // Ignore other events
                }
             }
             println!("Launcher process finished or detached.");
             Ok(()) // Assume success if spawn worked and process finished/detached
        }
        Err(e) => {
             eprintln!("Failed to spawn launcher: {}", e);
             // Check for the specific error here too
             if e.to_string().contains("os error 740") {
                 Err(format!("Failed to launch: The application requires administrator privileges. Try running GMM as administrator (not recommended). Error: {}", e))
             } else {
                  Err(format!("Failed to spawn executable: {}", e)) // Convert error to string
             }
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
            mod_count: 0,
            enabled_mod_count: None,
            recent_mod_count: None,
            favorite_mod_count: None,
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
            base_image: row.get(6)?, mod_count: row.get(7)?,
            enabled_mod_count: None,
            recent_mod_count: None,
            favorite_mod_count: None
        })
    }).map_err(|e| e.to_string())?;
    entity_iter.collect::<SqlResult<Vec<Entity>>>().map_err(|e| e.to_string())
}


#[command]
fn get_entity_details(entity_slug: String, db_state: State<DbState>) -> CmdResult<Entity> {
    println!("[get_entity_details] Starting for entity: {}", entity_slug);
    
    // PART 1: Get base entity info with a brief lock
    let entity_info = {
        let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
        let conn = &*conn_guard; // Dereference the guard
        
        let mut stmt = conn.prepare(
            "SELECT e.id, e.category_id, e.name, e.slug, e.description, e.details, e.base_image, COUNT(a.id) as mod_count
             FROM entities e LEFT JOIN assets a ON e.id = a.entity_id
             WHERE e.slug = ?1 GROUP BY e.id"
        ).map_err(|e| format!("[get_entity_details] DB prepare error: {}", e))?;
        
        // Get basic entity details first
        stmt.query_row(params![entity_slug], |row| {
             Ok(Entity {
                id: row.get(0)?, 
                category_id: row.get(1)?, 
                name: row.get(2)?,
                slug: row.get(3)?, 
                description: row.get(4)?, 
                details: row.get(5)?,
                base_image: row.get(6)?, 
                mod_count: row.get(7)?,
                enabled_mod_count: None,  // Will be populated later
                recent_mod_count: None,   // Will be populated later
                favorite_mod_count: None  // Will be populated later
            })
        }).map_err(|e| match e { 
            rusqlite::Error::QueryReturnedNoRows => format!("Entity '{}' not found", entity_slug),
            _ => format!("[get_entity_details] DB row error: {}", e),
        })?
    }; // conn_guard is released here
    
    // Create a mutable copy we'll update with the additional counts
    let mut entity = entity_info;
    
    // PART 2: Get folder paths from DB with a separate brief lock
    let asset_folder_paths: Vec<String> = {
        let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
        let conn = &*conn_guard;
        
        // Prepare statement and collect all folder paths while holding lock
        let mut stmt = conn.prepare("SELECT folder_name FROM assets WHERE entity_id = ?1")
            .map_err(|e| format!("[get_entity_details] Error preparing folder query: {}", e))?;
            
        let folder_iter = stmt.query_map(params![entity.id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("[get_entity_details] Error executing folder query: {}", e))?;
            
        // Collect all paths into a Vec to release the lock sooner
        let mut paths = Vec::new();
        for result in folder_iter {
            match result {
                Ok(path) => paths.push(path.replace("\\", "/")),
                Err(e) => println!("[get_entity_details] Warning: Error fetching path: {}", e),
            }
        }
        paths
    }; // conn_guard is released here
    
    // PART 3: Get mods base path (uses a lock internally, so call outside of any lock section)
    let base_mods_path = match get_mods_base_path_from_settings(&db_state) {
        Ok(path) => path,
        Err(e) => {
            println!("[get_entity_details] Warning: Error getting base mods path: {}", e);
            // We'll proceed with empty counts since we can't check the disk
            entity.enabled_mod_count = Some(0);
            entity.recent_mod_count = Some(0);
            entity.favorite_mod_count = Some(0);
            return Ok(entity);
        }
    };
    
    // PART 4: Count enabled mods by checking disk paths (NO DB LOCK NEEDED)
    let mut enabled_count = 0;
    for clean_relative_path_str in &asset_folder_paths {
        let clean_relative_path = PathBuf::from(clean_relative_path_str);
        let filename_osstr = match clean_relative_path.file_name() {
            Some(name) => name,
            None => continue, // Skip if we can't get filename
        };
        
        let filename_str = filename_osstr.to_string_lossy();
        if filename_str.is_empty() { continue; }
        
        // Check only enabled state path
        let full_path_if_enabled = base_mods_path.join(&clean_relative_path);
        if full_path_if_enabled.is_dir() {
            enabled_count += 1;
        }
    }
    entity.enabled_mod_count = Some(enabled_count);
    
    // PART 5: Get recent mod count and favorite counts with a final lock
    {
        let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
        let conn = &*conn_guard;
        
        // Count recent mods (approximation using ID sorting, assuming higher IDs are more recent)
        if entity.mod_count > 0 {
            match conn.query_row(
                "SELECT COUNT(*) FROM assets 
                 WHERE entity_id = ?1 
                 AND id > (SELECT MAX(id) - (COUNT(*) / 4) FROM assets WHERE entity_id = ?1)",
                params![entity.id],
                |row| row.get::<_, i32>(0),
            ) {
                Ok(count) => {
                    entity.recent_mod_count = Some(count);
                },
                Err(e) => {
                    println!("[get_entity_details] Warning: Error counting recent mods: {}", e);
                    entity.recent_mod_count = Some(0);
                }
            }
        } else {
            entity.recent_mod_count = Some(0);
        }
        
        // Count mods in favorite presets
        match conn.query_row(
            "SELECT COUNT(DISTINCT a.id) FROM assets a
             JOIN preset_assets pa ON a.id = pa.asset_id
             JOIN presets p ON pa.preset_id = p.id
             WHERE a.entity_id = ?1 AND p.is_favorite = 1",
            params![entity.id],
            |row| row.get::<_, i32>(0),
        ) {
            Ok(count) => {
                entity.favorite_mod_count = Some(count);
            },
            Err(e) => {
                println!("[get_entity_details] Warning: Error counting mods in favorite presets: {}", e);
                entity.favorite_mod_count = Some(0);
            }
        }
    } // Final conn_guard is released here
    
    println!("[get_entity_details] Completed for entity: {}", entity_slug);
    Ok(entity)
}

#[command]
fn get_assets_for_entity(entity_slug: String, db_state: State<DbState>, _app_handle: AppHandle) -> CmdResult<Vec<Asset>> {
    let base_mods_path = get_mods_base_path_from_settings(&db_state)
                             .map_err(|e| format!("[get_assets_for_entity {}] Error getting base mods path: {}", entity_slug, e))?;

    let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let conn = &*conn_guard;

    // --- Entity ID Lookup ---
    let entity_id: i64 = conn.query_row(
        "SELECT id FROM entities WHERE slug = ?1",
        params![entity_slug],
        |row| row.get(0),
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => format!("[get_assets_for_entity {}] Entity not found for assets lookup", entity_slug),
        _ => format!("[get_assets_for_entity {}] DB Error getting entity ID: {}", entity_slug, e),
    })?;

    // --- Prepare Statement ---
    let mut stmt = conn.prepare(
        "SELECT id, entity_id, name, description, folder_name, image_filename, author, category_tag
         FROM assets WHERE entity_id = ?1 ORDER BY name"
    ).map_err(|e| format!("[get_assets_for_entity {}] DB Error preparing asset statement: {}", entity_slug, e))?;

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

    match asset_rows_result {
        Ok(asset_iter) => {
             for (index, asset_result) in asset_iter.enumerate() {
                 match asset_result {
                     Ok(mut asset_from_db) => {
                         // --- Corrected State Detection Logic ---
                         // `asset_from_db.folder_name` currently holds the CLEAN relative path from DB
                         let clean_relative_path_from_db = PathBuf::from(&asset_from_db.folder_name);

                         // Construct potential paths based on the CLEAN relative path
                         let filename_osstr = clean_relative_path_from_db.file_name().unwrap_or_default();
                         let filename_str = filename_osstr.to_string_lossy();
                         if filename_str.is_empty() {
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

                         // Determine state based on which path exists
                         if full_path_if_enabled.is_dir() {
                             asset_from_db.is_enabled = true;
                             // Set folder_name to the actual path found on disk
                             asset_from_db.folder_name = clean_relative_path_from_db.to_string_lossy().replace("\\", "/");
                         } else if full_path_if_disabled.is_dir() {
                             asset_from_db.is_enabled = false;
                             // Set folder_name to the actual path found on disk (the disabled one)
                              let disabled_relative_path = match relative_parent_path {
                                 Some(parent) if parent.as_os_str().len() > 0 => parent.join(&disabled_filename),
                                 _ => PathBuf::from(&disabled_filename),
                              };
                             asset_from_db.folder_name = disabled_relative_path.to_string_lossy().replace("\\", "/");
                         } else {
                             // Mod folder doesn't exist in either state
                             continue; // Skip this asset
                         }

                         assets_to_return.push(asset_from_db);
                         // --- End Corrected State Detection ---
                     }
                     Err(e) => {
                         eprintln!("[get_assets_for_entity {}] Error processing asset row index {}: {}", entity_slug, index, e);
                     }
                 }
             }
        }
        Err(e) => {
             let err_msg = format!("[get_assets_for_entity {}] DB Error preparing asset iterator: {}", entity_slug, e);
             return Err(err_msg);
        }
    }

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
    asset_id: i64,
    db_state: State<DbState>
) -> CmdResult<String> {
    // --- Data needed from DB ---
    let base_mods_path_str: String;
    let clean_relative_path_str: String;
    let image_filename: String;

    // --- Acquire lock *only* for DB reads ---
    { // Scope for the MutexGuard
        println!("[get_asset_image_path ID: {}] Acquiring DB lock...", asset_id);
        let conn_guard = db_state.0.lock().map_err(|_| format!("[get_asset_image_path ID: {}] DB lock poisoned", asset_id))?;
        let conn = &*conn_guard;

        // 1. Get base mods path from settings
        base_mods_path_str = get_setting_value(conn, SETTINGS_KEY_MODS_FOLDER)
            .map_err(|e| format!("[get_asset_image_path ID: {}] DB Error getting base path: {}", asset_id, e))?
            .ok_or_else(|| format!("[get_asset_image_path ID: {}] Mods folder path not set", asset_id))?;

        // 2. Fetch asset info (clean path and image filename) using asset_id
        let (fetched_path, fetched_image_opt): (String, Option<String>) = conn.query_row(
            "SELECT folder_name, image_filename FROM assets WHERE id = ?1",
            params![asset_id],
            |row| Ok((row.get(0)?, row.get(1)?))
        ).map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => format!("[get_asset_image_path ID: {}] Asset not found.", asset_id),
            _ => format!("[get_asset_image_path ID: {}] DB Error getting asset info: {}", asset_id, e),
        })?;

        clean_relative_path_str = fetched_path.replace("\\", "/"); // Normalize path separators immediately
        image_filename = match fetched_image_opt {
             Some(name) if !name.is_empty() => name,
             _ => {
                 // If no image filename in DB, we can stop early. Release lock implicitly.
                 return Err(format!("[get_asset_image_path ID: {}] Asset does not have an associated image filename.", asset_id));
             }
        };

        println!("[get_asset_image_path ID: {}] Releasing DB lock.", asset_id);
        // MutexGuard `conn_guard` is dropped here, releasing the lock
    }
    // --- Lock is released ---

    // --- Filesystem operations (No DB lock needed) ---
    println!("[get_asset_image_path ID: {}] Performing filesystem checks...", asset_id);
    let base_mods_path = PathBuf::from(base_mods_path_str);
    let clean_relative_path_buf = PathBuf::from(&clean_relative_path_str); // Already normalized

    // 3. Determine current folder path (enabled or disabled)
    let mod_folder_filename_osstr = clean_relative_path_buf.file_name()
        .ok_or_else(|| format!("[get_asset_image_path ID: {}] Cannot get folder filename from '{}'", asset_id, clean_relative_path_str))?;
    let mod_folder_filename_str = mod_folder_filename_osstr.to_string_lossy();
    let disabled_mod_folder_filename = format!("{}{}", DISABLED_PREFIX, mod_folder_filename_str);
    let relative_parent_path = clean_relative_path_buf.parent();

    let full_path_if_enabled = base_mods_path.join(&clean_relative_path_buf);
    let full_path_if_disabled = match relative_parent_path {
        Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_mod_folder_filename),
        _ => base_mods_path.join(&disabled_mod_folder_filename),
    };

    // Use is_dir which might be slightly faster than exists() if we only care about directories
    let current_mod_folder_path = if full_path_if_enabled.is_dir() {
        println!("[get_asset_image_path ID: {}] Found enabled path: {}", asset_id, full_path_if_enabled.display());
        full_path_if_enabled
    } else if full_path_if_disabled.is_dir() {
        println!("[get_asset_image_path ID: {}] Found disabled path: {}", asset_id, full_path_if_disabled.display());
        full_path_if_disabled
    } else {
        // Folder not found. This isn't necessarily an error for *this* function,
        // but we can't construct the image path. Return an error.
        println!("[get_asset_image_path ID: {}] Mod folder not found on disk.", asset_id);
        return Err(format!("Mod folder for asset ID {} not found on disk (Checked '{}' and '{}').", asset_id, full_path_if_enabled.display(), full_path_if_disabled.display()));
    };

    // 4. Construct the FULL path to the image file within the found folder
    let image_full_path = current_mod_folder_path.join(&image_filename);
    println!("[get_asset_image_path ID: {}] Checking image file: {}", asset_id, image_full_path.display());

    // 5. Check if the image file *itself* exists
    if !image_full_path.is_file() {
        println!("[get_asset_image_path ID: {}] Image file does not exist.", asset_id);
        return Err(format!("Image file '{}' not found in mod folder '{}'.", image_filename, current_mod_folder_path.display()));
    }

    // Return the absolute path string for the frontend
    println!("[get_asset_image_path ID: {}] Success, returning path: {}", asset_id, image_full_path.display());
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
    println!("Starting robust mod directory scan with pruning...");
    let base_mods_path = get_mods_base_path_from_settings(&db_state).map_err(|e| e.to_string())?;
    println!("Scanning base path: {}", base_mods_path.display());

    if !base_mods_path.is_dir() {
        let err_msg = format!("Mods directory path is not a valid directory: {}", base_mods_path.display());
        app_handle.emit_all(SCAN_ERROR_EVENT, &err_msg).unwrap_or_else(|e| eprintln!("Failed to emit scan error event: {}", e));
        return Err(err_msg);
    }

    // --- Preparation ---
    let deduction_maps = {
        let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
        let conn = &*conn_guard;
        fetch_deduction_maps(conn).map_err(|e| format!("Failed to pre-fetch deduction maps: {}", e))?
    };
    println!("[Scan Prep] Deduction maps loaded.");

    let db_path = {
        let data_dir = get_app_data_dir(&app_handle).map_err(|e| e.to_string())?;
        data_dir.join(DB_NAME)
    };
    let db_path_str = db_path.to_string_lossy().to_string();
    let base_mods_path_clone = base_mods_path.clone();
    let app_handle_clone = app_handle.clone();
    let maps_clone = deduction_maps.clone();

    println!("[Scan Prep] Calculating total potential mod folders...");
    let potential_mod_folders_for_count: Vec<PathBuf> = WalkDir::new(&base_mods_path)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok().filter(|entry| entry.file_type().is_dir()))
        .filter(|e| {
             // Temporary check for rename condition as well for count (might be slightly inaccurate if rename fails later)
             let path = e.path();
             let filename = path.file_name().unwrap_or_default().to_string_lossy();
             // Check for INI OR if it needs renaming (so it's counted)
             has_ini_file(&path.to_path_buf()) || (filename.starts_with("DISABLED") && !filename.starts_with(DISABLED_PREFIX))
         })
        .map(|e| e.path().to_path_buf())
        .collect();
    let total_to_process = potential_mod_folders_for_count.len();
    println!("[Scan Prep] Found {} potential mod folders for progress total (includes folders needing rename).", total_to_process);

    app_handle.emit_all(SCAN_PROGRESS_EVENT, ScanProgress {
            processed: 0, total: total_to_process, current_path: None, message: "Starting scan...".to_string()
        }).unwrap_or_else(|e| eprintln!("Failed to emit initial scan progress: {}", e));


    // --- Process folders and collect FOUND asset IDs in a blocking task ---
    let scan_task = async_runtime::spawn_blocking(move || {
        // Open a new connection inside the blocking task
        let conn = Connection::open(&db_path_str).map_err(|e| format!("Failed to open DB connection in scan task: {}", e))?;

        // --- Fetch ALL asset IDs and their CLEAN relative paths from DB first ---
        let mut initial_db_assets = HashMap::<i64, String>::new(); // asset_id -> clean_relative_path
        { // Scope for the statement
            let mut stmt = conn.prepare("SELECT id, folder_name FROM assets")
                .map_err(|e| format!("Failed to prepare asset fetch statement: {}", e))?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)));
             let row_iter = rows.map_err(|e| format!("Error creating asset query iterator: {}", e))?;
            for row_result in row_iter {
                 match row_result {
                     Ok((id, folder_name)) => {
                         initial_db_assets.insert(id, folder_name.replace("\\", "/"));
                     }
                     Err(e) => {
                          eprintln!("[Scan Task Prep] Error fetching asset row from DB: {}", e);
                     }
                 }
            }
        }
        println!("[Scan Task Prep] Fetched {} assets from DB initially.", initial_db_assets.len());

        let mut processed_count = 0; // Counts folders *identified* as mods and processed
        let mut mods_added_count = 0;
        let mut mods_updated_count = 0;
        let mut errors_count = 0;
        let mut processed_mod_paths = HashSet::new(); // Track processed paths to avoid duplicates if structure is odd
        let mut found_asset_ids = HashSet::<i64>::new(); // Track IDs found on disk
        let mut renamed_count = 0; // Count renamed folders

        // --- Iterate using WalkDir ---
        let mut walker = WalkDir::new(&base_mods_path_clone).min_depth(1).into_iter();

        while let Some(entry_result) = walker.next() {
            match entry_result {
                Ok(entry) => {
                    // Use mutable path as it might be changed by rename logic
                    let mut current_path = entry.path().to_path_buf();
                    let is_directory = entry.file_type().is_dir(); // Check type once

                    if is_directory && !processed_mod_paths.contains(&current_path) {
                        // --- START: Check for DISABLED without underscore and rename ---
                        let filename_osstr = current_path.file_name().unwrap_or_default();
                        let filename_str = filename_osstr.to_string_lossy();

                        let needs_rename = filename_str.starts_with("DISABLED") && !filename_str.starts_with(DISABLED_PREFIX);
                        let mut current_path_for_processing = current_path.clone(); // Path to use for has_ini and processing

                        if needs_rename {
                            let new_filename = format!("{}{}", DISABLED_PREFIX, filename_str.strip_prefix("DISABLED").unwrap_or(&filename_str));
                            if let Some(parent_path) = current_path.parent() {
                                let new_path = parent_path.join(&new_filename);
                                println!("[Scan Task - Rename] Found incorrect prefix: '{}'. Renaming to '{}'", current_path.display(), new_path.display());

                                // Emit progress before rename attempt
                                app_handle_clone.emit_all(SCAN_PROGRESS_EVENT, ScanProgress {
                                     processed: processed_count, // Don't increment processed count for rename yet
                                     total: total_to_process,
                                     current_path: Some(current_path.display().to_string()),
                                     message: format!("Renaming: {}", filename_str)
                                }).unwrap_or_else(|e| eprintln!("Failed to emit rename progress: {}", e));

                                match fs::rename(&current_path, &new_path) {
                                    Ok(_) => {
                                        println!("[Scan Task - Rename] Successfully renamed.");
                                        current_path_for_processing = new_path; // Use the NEW path for further processing
                                        renamed_count += 1;
                                    }
                                    Err(e) => {
                                        eprintln!("[Scan Task - Rename] ERROR: Failed to rename folder '{}': {}. Skipping folder.", current_path.display(), e);
                                        errors_count += 1;
                                        // Don't process this folder if rename failed
                                        walker.skip_current_dir(); // Skip children as well
                                        continue; // Move to the next entry in WalkDir
                                    }
                                }
                            } else {
                                eprintln!("[Scan Task - Rename] ERROR: Cannot get parent path for '{}'. Skipping rename and folder.", current_path.display());
                                errors_count += 1;
                                walker.skip_current_dir(); // Skip children
                                continue; // Move to the next entry
                            }
                        }
                        // --- END: Rename Check ---

                        // Now check if the (potentially renamed) folder has an INI file
                        if has_ini_file(&current_path_for_processing) {
                            // This is a mod folder (or was successfully renamed to be treated as one)
                            processed_count += 1; // Increment processed count *here*
                            processed_mod_paths.insert(current_path_for_processing.clone()); // Add the path we actually processed
                            let path_display = current_path_for_processing.display().to_string();
                            let folder_name_only = current_path_for_processing.file_name().unwrap_or_default().to_string_lossy();

                            // Emit progress for actual mod processing
                            app_handle_clone.emit_all(SCAN_PROGRESS_EVENT, ScanProgress {
                                processed: processed_count,
                                total: total_to_process,
                                current_path: Some(path_display.clone()),
                                message: format!("Processing: {}", folder_name_only)
                            }).unwrap_or_else(|e| eprintln!("Failed to emit scan progress: {}", e));

                            // --- Start Original Deduction/DB Logic (using current_path_for_processing) ---
                            match deduce_mod_info_v2(&current_path_for_processing, &base_mods_path_clone, &maps_clone) {
                                Some(deduced) => {
                                    println!("[Scan Task] Deduced slug for '{}': {}", path_display, deduced.entity_slug);
                                    let target_entity_id_result: Option<i64> = maps_clone.entity_slug_to_id.get(&deduced.entity_slug).copied();

                                    if let Some(target_entity_id) = target_entity_id_result {
                                        println!("[Scan Task] Found entity ID {} for slug '{}'", target_entity_id, deduced.entity_slug);

                                        let relative_path_buf = match current_path_for_processing.strip_prefix(&base_mods_path_clone) {
                                            Ok(p) => p.to_path_buf(),
                                            Err(_) => {
                                                eprintln!("[Scan Task] Error: Could not strip base path prefix from '{}'. Skipping.", path_display);
                                                errors_count += 1;
                                                continue; // Skip only this mod folder deduction/DB part
                                            }
                                        };

                                        let filename_osstr = relative_path_buf.file_name().unwrap_or_default();
                                        let filename_str = filename_osstr.to_string_lossy();
                                        // --- Critical: Ensure stripping the CORRECT prefix after potential rename ---
                                        let clean_filename = filename_str.strip_prefix(DISABLED_PREFIX).unwrap_or(&filename_str);
                                        // ---
                                        let relative_parent_path = relative_path_buf.parent();
                                        let relative_path_to_store = match relative_parent_path {
                                            Some(parent) if parent.as_os_str().len() > 0 => parent.join(clean_filename).to_string_lossy().to_string(),
                                            _ => clean_filename.to_string(),
                                        };
                                        let relative_path_to_store = relative_path_to_store.replace("\\", "/");
                                        println!("[Scan Task] Calculated DB path: '{}'", relative_path_to_store);

                                        let existing_db_asset_id: Option<i64> = conn.query_row(
                                            "SELECT id FROM assets WHERE entity_id = ?1 AND folder_name = ?2",
                                            params![target_entity_id, relative_path_to_store],
                                            |row| row.get(0),
                                        ).optional().map_err(|e| format!("DB error checking for existing asset '{}': {}", relative_path_to_store, e))?;

                                        if let Some(asset_id) = existing_db_asset_id {
                                            println!("[Scan Task] Asset already in DB (ID: {}), path '{}'. Marking as found.", asset_id, relative_path_to_store);
                                            found_asset_ids.insert(asset_id);
                                            // mods_updated_count += 1; // Optional update logic here
                                        } else {
                                            println!("[Scan Task] Inserting new asset: EntityID={}, Name='{}', Path='{}'", target_entity_id, deduced.mod_name, relative_path_to_store);
                                            let insert_result = conn.execute(
                                                "INSERT INTO assets (entity_id, name, description, folder_name, image_filename, author, category_tag) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                                                params![
                                                    target_entity_id,
                                                    deduced.mod_name,
                                                    deduced.description,
                                                    relative_path_to_store,
                                                    deduced.image_filename,
                                                    deduced.author,
                                                    deduced.mod_type_tag
                                                ]
                                            );

                                            match insert_result {
                                                Ok(changes) => {
                                                    if changes > 0 {
                                                        mods_added_count += 1;
                                                        let new_id = conn.last_insert_rowid();
                                                        found_asset_ids.insert(new_id);
                                                        println!("[Scan Task]   -> Insert successful (New ID: {})", new_id);
                                                    } else {
                                                        eprintln!("[Scan Task]   -> Insert reported 0 changes for '{}'.", relative_path_to_store);
                                                        errors_count += 1;
                                                    }
                                                }
                                                Err(e) => {
                                                    if e.to_string().contains("UNIQUE constraint failed: assets.folder_name") {
                                                        eprintln!("[Scan Task]   -> Insert failed due to UNIQUE constraint on folder_name '{}'. Asset might exist under a different entity or needs pruning. Skipping insert.", relative_path_to_store);
                                                        // Maybe don't count as error if pruning will fix it?
                                                    } else {
                                                        eprintln!("[Scan Task]   -> DB error inserting new asset '{}': {}", relative_path_to_store, e);
                                                        errors_count += 1;
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        eprintln!("[Scan Task] CRITICAL ERROR: Deduced slug '{}' for path '{}' does NOT exist in the entity map! Skipping mod. Check DB initialization and deduction logic.", deduced.entity_slug, path_display);
                                        errors_count += 1;
                                    }
                                }
                                None => {
                                    eprintln!("[Scan Task] Error: Failed to deduce mod info for path '{}'", path_display);
                                    errors_count += 1;
                                }
                            }
                            // --- End Original Deduction/DB Logic ---
                            walker.skip_current_dir(); // Skip children after processing a mod folder
                        }
                        // If it's a directory but doesn't have an INI (and wasn't renamed+processed),
                        // we just let WalkDir continue into its children.
                    }
                    // If it's not a directory, or already processed, ignore.
                }
                Err(e) => {
                     eprintln!("[Scan Task] Error accessing path during scan: {}", e);
                     errors_count += 1;
                }
            }
        }

        // --- Pruning Logic (Remains the same) ---
        let mut mods_to_prune_ids = Vec::new();
        for (asset_id, _clean_path) in initial_db_assets.iter() {
            if !found_asset_ids.contains(asset_id) {
                 mods_to_prune_ids.push(*asset_id);
            }
        }
        let prune_count = mods_to_prune_ids.len();
        let mut pruned_count = 0;
        let mut pruning_errors_count = 0;

        if !mods_to_prune_ids.is_empty() {
            println!("[Scan Task Pruning] Found {} mods in DB missing from disk. Pruning...", prune_count);
            app_handle_clone.emit_all(PRUNING_START_EVENT, prune_count).ok();

             let ids_to_delete_sql: Vec<Box<dyn rusqlite::ToSql>> = mods_to_prune_ids
                .into_iter()
                .map(|id| Box::new(id) as Box<dyn rusqlite::ToSql>)
                .collect();

            if !ids_to_delete_sql.is_empty() {
                let placeholders = ids_to_delete_sql.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!("DELETE FROM assets WHERE id IN ({})", placeholders);

                app_handle_clone.emit_all(PRUNING_PROGRESS_EVENT, format!("Deleting {} entries...", ids_to_delete_sql.len())).ok();

                let delete_result = conn.execute(&sql, rusqlite::params_from_iter(ids_to_delete_sql))
                                        .map_err(|e| format!("DB error during pruning: {}", e));

                match delete_result {
                    Ok(count) => {
                         pruned_count = count;
                         println!("[Scan Task Pruning] Successfully pruned {} asset entries.", pruned_count);
                         app_handle_clone.emit_all(PRUNING_COMPLETE_EVENT, pruned_count).ok();
                    },
                    Err(e) => {
                        eprintln!("[Scan Task Pruning] {}", e);
                         pruning_errors_count += 1;
                         app_handle_clone.emit_all(PRUNING_ERROR_EVENT, e).ok();
                    }
                }
            } else {
                 println!("[Scan Task Pruning] No valid IDs to prune after conversion.");
                 app_handle_clone.emit_all(PRUNING_COMPLETE_EVENT, 0).ok();
            }
        } else {
             println!("[Scan Task Pruning] No missing mods found. Skipping pruning.");
        }
        // --- End Pruning Logic ---

        let total_errors = errors_count + pruning_errors_count;
        // Return renamed_count as well
        Ok::<_, String>((processed_count, mods_added_count, mods_updated_count, total_errors, pruned_count, renamed_count))
    });

    // --- Handle Task Result ---
     match scan_task.await {
         Ok(Ok((processed, added, _updated, errors, pruned, renamed))) => { // Add renamed here
             let rename_msg = if renamed > 0 { format!(" Renamed {} incorrectly prefixed folders.", renamed) } else { "".to_string() };
             let summary = format!(
                 "Scan complete. Processed {} mod folders. Added {} new mods. Pruned {} missing mods.{} {} errors occurred.",
                 processed, added, pruned, rename_msg, errors
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
    image_data: Option<Vec<u8>>,
    new_target_entity_slug: Option<String>,
    db_state: State<DbState>
) -> CmdResult<()> { // Returns Result<(), String>
    println!("[update_asset_info] Start for asset ID: {}. Relocate to: {:?}. Image Data Provided: {}",
        asset_id, new_target_entity_slug, image_data.is_some());

    let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let conn = &*conn_guard;

    // --- 1. Get Current Asset Location Info ---
    let current_info = get_asset_location_info(conn, asset_id)
        .map_err(|e| format!("Failed get current asset info: {}", e))?;
    println!("[update_asset_info] Current Info: {:?}", current_info);

    // --- 2. Relocation Logic ---
    let needs_relocation = new_target_entity_slug.is_some() && new_target_entity_slug.as_deref() != Some(&current_info.entity_slug);
    let mut final_entity_id = current_info.entity_id;
    let mut final_relative_path_str = current_info.clean_relative_path.clone();
    let mut final_path_on_disk: Option<PathBuf> = None;

    let base_mods_path = PathBuf::from(
        get_setting_value(conn, SETTINGS_KEY_MODS_FOLDER)
           .map_err(|e|e.to_string())?
           .ok_or_else(|| "Mods folder path not set".to_string())?
    );
    println!("[update_asset_info] Base mods path: {}", base_mods_path.display());

    if needs_relocation {
        // ... (setup for relocation: target_slug, new_entity_id, etc.) ...
        let target_slug = new_target_entity_slug.as_ref().unwrap();
        let (new_entity_id, new_category_slug): (i64, String) = conn.query_row(
            "SELECT e.id, c.slug FROM entities e JOIN categories c ON e.category_id = c.id WHERE e.slug = ?1",
            params![target_slug],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(|e| format!("DB Error getting new target entity info: {}", e))?;

        // --- Determine Current Full Path on Disk (Check Enabled/Disabled) ---
        let current_relative_path_buf = PathBuf::from(&current_info.clean_relative_path);
        let current_filename_osstr = current_relative_path_buf.file_name().ok_or("Cannot get current filename")?;
        let current_filename_str = current_filename_osstr.to_string_lossy();
        let disabled_filename = format!("{}{}", DISABLED_PREFIX, current_filename_str);
        let relative_parent_path = current_relative_path_buf.parent();
        let full_path_if_enabled = base_mods_path.join(&current_relative_path_buf);
        let full_path_if_disabled = match relative_parent_path {
           Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
           _ => base_mods_path.join(&disabled_filename),
        };
        let current_full_path = if full_path_if_enabled.is_dir() { full_path_if_enabled }
            else if full_path_if_disabled.is_dir() { full_path_if_disabled }
            else { return Err(format!("Cannot relocate: Source folder not found at '{}' or disabled variant.", full_path_if_enabled.display())); };
        println!("[update_asset_info] Current full path on disk: {}", current_full_path.display());

        // --- Construct New Relative (for DB) and Full (for Disk) Paths ---
        let mod_base_name = current_filename_str.trim_start_matches(DISABLED_PREFIX);
        let new_relative_path_buf = PathBuf::new().join(&new_category_slug).join(target_slug).join(mod_base_name);
        final_relative_path_str = new_relative_path_buf.to_string_lossy().replace("\\", "/"); // For DB

        // Determine the name to use on disk (keep disabled prefix if present)
        let new_filename_to_use_on_disk = if current_full_path.file_name().map_or(false, |name| name.to_string_lossy().starts_with(DISABLED_PREFIX)) {
             disabled_filename // Keep disabled prefix
        } else {
             mod_base_name.to_string() // Use clean name
        };
        let new_full_dest_path_on_disk = base_mods_path.join(&new_category_slug).join(target_slug).join(&new_filename_to_use_on_disk);
        println!("[update_asset_info] New relative path for DB: {}", final_relative_path_str);
        println!("[update_asset_info] New full destination path on disk: {}", new_full_dest_path_on_disk.display());

        // --- Create Parent Directory & Perform Move ---
        if let Some(parent) = new_full_dest_path_on_disk.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())? // Add map_err
       } else {
            return Err("Could not determine parent for new path".into());
       }
        if new_full_dest_path_on_disk.exists() { return Err(format!("Cannot relocate: Target path '{}' already exists.", new_full_dest_path_on_disk.display())); }
        fs::rename(&current_full_path, &new_full_dest_path_on_disk)
            .map_err(|e| e.to_string())?; // Add map_err
        // --- END FIX 2 ---

        println!("[update_asset_info] Successfully moved mod folder.");

        final_entity_id = new_entity_id;
        final_path_on_disk = Some(new_full_dest_path_on_disk);
    }

    // --- 4. Handle Image Saving (Handles Paste > File Path > Existing) ---

    // Determine the mod folder path ON DISK where the image should be saved
    // This uses the path *after* potential relocation if it happened.
    let mod_folder_on_disk = if let Some(relocated_path) = final_path_on_disk {
        relocated_path
    } else {
        // If no relocation, determine current path (enabled/disabled) based on current_info
        let current_relative_path_buf = PathBuf::from(&current_info.clean_relative_path);
        let current_filename_osstr = current_relative_path_buf.file_name().ok_or("Cannot get current filename")?;
        let current_filename_str = current_filename_osstr.to_string_lossy();
        let disabled_filename = format!("{}{}", DISABLED_PREFIX, current_filename_str);
        let relative_parent_path = current_relative_path_buf.parent();
        let full_path_if_enabled = base_mods_path.join(&current_relative_path_buf);
        let full_path_if_disabled = match relative_parent_path {
            Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
            _ => base_mods_path.join(&disabled_filename),
        };
        if full_path_if_enabled.is_dir() { full_path_if_enabled }
        else if full_path_if_disabled.is_dir() { full_path_if_disabled }
        else { return Err(format!("Mod folder not found on disk at '{}' or disabled variant.", full_path_if_enabled.display())); }
    };
    println!("[update_asset_info] Confirmed mod path on disk for image: {}", mod_folder_on_disk.display());

    // Ensure the target directory exists (it should, but double-check)
    if !mod_folder_on_disk.is_dir() {
        // This might happen if the folder got deleted between checks, try creating it.
        println!("[update_asset_info] Warning: Target mod folder {} does not exist, attempting to create.", mod_folder_on_disk.display());
        fs::create_dir_all(&mod_folder_on_disk).map_err(|e| e.to_string())?;
    }

    let mut image_filename_to_save: Option<String> = None; // Default to None

    // --- Priority 1: Handle pasted/provided image data ---
    if let Some(data) = image_data {
        println!("[update_asset_info] Handling provided image data ({} bytes)", data.len());
        let target_image_path = mod_folder_on_disk.join(TARGET_IMAGE_FILENAME);
        // Use fs::write which creates/truncates the file
        fs::write(&target_image_path, data)
            .map_err(|e| format!("Failed to save pasted image data to '{}': {}", target_image_path.display(), e))?;
        println!("[update_asset_info] Image data written successfully.");
        image_filename_to_save = Some(TARGET_IMAGE_FILENAME.to_string());
    }
    // --- Priority 2: Handle selected file path (only if no data was provided) ---
    else if let Some(source_path_str) = selected_image_absolute_path {
        println!("[update_asset_info] Handling selected image file path: {}", source_path_str);
        let source_path = PathBuf::from(&source_path_str);
        if !source_path.is_file() { return Err(format!("Selected image file does not exist: {}", source_path.display())); }
        let target_image_path = mod_folder_on_disk.join(TARGET_IMAGE_FILENAME);
        fs::copy(&source_path, &target_image_path)
             .map_err(|e| format!("Failed to copy selected image to '{}': {}", target_image_path.display(), e))?;
        println!("[update_asset_info] Image file copied successfully.");
        image_filename_to_save = Some(TARGET_IMAGE_FILENAME.to_string());
    }
    // --- Priority 3: No new image provided, fetch existing filename from DB ---
    else {
         println!("[update_asset_info] No new image data or path provided. Fetching existing filename.");
         // Query existing filename. Ok if it doesn't exist (returns None)
         image_filename_to_save = conn.query_row::<Option<String>, _, _>(
            "SELECT image_filename FROM assets WHERE id=?1",
             params![asset_id],
             |r|r.get(0)
         ).optional().map_err(|e| format!("DB error fetching existing image name: {}", e))?.flatten(); // flatten Option<Option<String>>
    }
    println!("[update_asset_info] Image handling complete. Filename to save in DB: {:?}", image_filename_to_save);


    // --- 5. Update Database ---
    println!("[update_asset_info] Attempting DB update for asset ID {}...", asset_id);
    let changes = conn.execute(
        "UPDATE assets SET name = ?1, description = ?2, author = ?3, category_tag = ?4, image_filename = ?5, entity_id = ?6, folder_name = ?7 WHERE id = ?8",
        params![
            name, // Use name from arguments
            description,
            author,
            category_tag,
            image_filename_to_save, // Use the determined filename
            final_entity_id,        // Use potentially updated entity ID
            final_relative_path_str, // Use potentially updated relative path (for DB only)
            asset_id
        ]
    ).map_err(|e| format!("Failed update asset info in DB for ID {}: {}", asset_id, e))?;

    println!("[update_asset_info] DB update executed. Changes: {}", changes);
    if changes == 0 { eprintln!("[update_asset_info] Warning: DB update affected 0 rows for asset ID {}.", asset_id); }

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
        // --- Update Filter ---
        .add_filter("Archives", &["zip", "7z", "rar"])
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
fn analyze_archive(
    file_path_str: String,
    // *** ADDED: Inject DB State ***
    db_state: State<DbState>
) -> CmdResult<ArchiveAnalysisResult> {
    println!("[analyze_archive] Analyzing: {}", file_path_str);
    let file_path = PathBuf::from(&file_path_str);
    if !file_path.is_file() { return Err(format!("Archive file not found: {}", file_path.display())); }

    let extension = file_path.extension().and_then(|os| os.to_str()).map(|s| s.to_lowercase());
    println!("[analyze_archive] Detected extension: {:?}", extension);

    let mut entries = Vec::new();
    let mut ini_contents: HashMap<String, String> = HashMap::new();
    let preview_candidates = ["preview.png", "icon.png", "thumbnail.png", "preview.jpg", "icon.jpg", "thumbnail.jpg"];

    // --- Fetch Deduction Maps ---
    let maps = {
        // Use a block to limit the scope of the lock guard
        let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
        let conn = &*conn_guard; // Dereference the guard
        fetch_deduction_maps(conn)
             .map_err(|e| format!("Analyze: Failed to fetch deduction maps: {}", e))?
    };
    println!("[analyze_archive] Deduction maps loaded.");
    // --- End Fetch ---

    match extension.as_deref() {
        Some("zip") => {
            println!("[analyze_archive] Processing as ZIP...");
            let file = fs::File::open(&file_path)
                .map_err(|e| format!("Failed to open zip file {}: {}", file_path.display(), e))?;
            let mut archive = ZipArchive::new(file)
                .map_err(|e| format!("Failed to read zip archive {}: {}", file_path.display(), e))?;

            for i in 0..archive.len() {
                let mut file_entry = archive.by_index(i)
                     .map_err(|e| format!("Failed to read zip entry #{}: {}", i, e))?;
                let path_str_opt = file_entry.enclosed_name().map(|p| p.to_string_lossy().replace("\\", "/"));
                if path_str_opt.is_none() { continue; }
                // --- FIX: Just clone the String if needed, or use directly ---
                let path_str = path_str_opt.unwrap().to_string(); // Use to_string() to ensure it's owned String
                let is_dir = file_entry.is_dir();

                if !is_dir && path_str.to_lowercase().ends_with(".ini") {
                    let mut content = String::new();
                    if file_entry.read_to_string(&mut content).is_ok() {
                        ini_contents.insert(path_str.clone(), content);
                    }
                }
                entries.push(ArchiveEntry { path: path_str, is_dir, is_likely_mod_root: false });
            }
        }
        Some("7z") => {
            println!("[analyze_archive] Processing as 7z...");
            // --- FIX: Use Password::empty() ---
            let mut archive = sevenz_rust::SevenZReader::open(&file_path_str, Password::empty())
                .map_err(|e| format!("Failed to open/read 7z archive {}: {}", file_path.display(), e))?;

             // --- FIX: Use for_each_entries ---
             archive.for_each_entries(|entry, reader| {
                let path_str = entry.name().replace("\\", "/");
                let is_dir = entry.is_directory();

                if !is_dir && path_str.to_lowercase().ends_with(".ini") {
                     let mut content_bytes = Vec::new();
                     let mut buffer = [0u8; 4096];
                     loop {
                        let bytes_read = reader.read(&mut buffer)?;
                        if bytes_read == 0 { break; }
                        content_bytes.extend_from_slice(&buffer[..bytes_read]);
                    }
                     let content = String::from_utf8_lossy(&content_bytes).to_string();
                     ini_contents.insert(path_str.clone(), content);
                }
                entries.push(ArchiveEntry { path: path_str, is_dir, is_likely_mod_root: false });
                Ok(true) // Continue processing entries
             })
             // --- Map the specific error type from the closure if needed ---
             .map_err(|e: sevenz_rust::Error| format!("Error iterating 7z entries: {}", e))?;
        }
        Some("rar") => {
            println!("[analyze_archive] Processing as RAR...");
            let mut list_archive = Archive::new(&file_path_str)
                .open_for_listing()
                .map_err(|e| e.to_string())?;

            let mut header_infos = Vec::new();
            // Iterate through headers
            for entry_result in &mut list_archive { // Keep iterating with &mut
                match entry_result {
                    Ok(header) => {
                        let path_str = header.filename.to_string_lossy().replace("\\", "/").to_string();
                        let is_dir = header.is_directory();
                        // --- FIX 1: Clone path_str for the first push ---
                        header_infos.push((path_str.clone(), is_dir, header.filename.clone()));
                        // --- End Fix 1 ---
                        entries.push(ArchiveEntry { path: path_str, is_dir, is_likely_mod_root: false });
                    }
                    Err(e) => {
                        eprintln!("[analyze_archive] Warning: Skipping RAR entry due to header read error: {}", e);
                        // --- FIX 2: Remove force_heal call ---
                        // list_archive.force_heal(); // Cannot call this here
                        // --- End Fix 2 ---
                        // The loop will continue to the next entry if possible,
                        // or stop if the error was fatal for the iterator.
                    }
                }
            }
            // `list_archive` borrow ends here

            // --- Rest of the RAR logic (re-opening for INI reading) remains the same ---
            let ini_files_to_read: Vec<(String, PathBuf)> = header_infos.iter()
               .filter(|(path, is_dir, _)| !*is_dir && path.to_lowercase().ends_with(".ini"))
               .map(|(path, _, original_filename)| (path.clone(), original_filename.clone()))
               .collect();

            if !ini_files_to_read.is_empty() {
               let mut processing_archive = Archive::new(&file_path_str).open_for_processing()
                    .map_err(|e| e.to_string())?;
               let mut read_count = 0;
               loop {
                   match processing_archive.read_header().map_err(|e| e.to_string())? {
                       Some(header_state) => {
                           let current_filename = header_state.entry().filename.clone();
                           let path_str = current_filename.to_string_lossy().replace("\\", "/").to_string();
                           if let Some(pos) = ini_files_to_read.iter().position(|(_, fname)| fname == &current_filename) {
                               match header_state.read() {
                                   Ok((bytes, next_state)) => {
                                       ini_contents.insert(path_str, String::from_utf8_lossy(&bytes).to_string());
                                       processing_archive = next_state;
                                       read_count += 1;
                                       if read_count == ini_files_to_read.len() { break; }
                                   }
                                   Err(e) => { return Err(format!("Error reading content of RAR INI '{}': {}", path_str, e)); }
                               }
                           } else {
                               processing_archive = header_state.skip().map_err(|e| e.to_string())?;
                           }
                       }
                       None => break,
                   }
               }
            }
        }
        _ => {
            return Err(format!("Unsupported archive type: {:?}", extension));
        }
    }
    println!("[analyze_archive] Pass 1: Found {} entries. Found {} INI files.", entries.len(), ini_contents.len());

    entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));

    // ... (Pass 2: Find roots) ...
    let mut likely_root_indices = HashSet::new();
    for (ini_index, ini_entry) in entries.iter().enumerate() {
        if !ini_entry.is_dir && ini_entry.path.to_lowercase().ends_with(".ini") {
            let parent_path_obj = Path::new(&ini_entry.path).parent();
            if let Some(parent_path_ref) = parent_path_obj {
                let parent_path_str_norm = parent_path_ref.to_string_lossy().replace("\\", "/");
                if parent_path_str_norm.is_empty() { continue; }
                let found_parent = entries.iter().position(|dir_entry| {
                    if !dir_entry.is_dir { return false; }
                    let dir_entry_path_norm = dir_entry.path.strip_suffix('/').unwrap_or(&dir_entry.path);
                    dir_entry_path_norm == parent_path_str_norm
                });
                if let Some(parent_index) = found_parent {
                    likely_root_indices.insert(parent_index);
                }
            }
        }
    }
    // ... (Pass 3: Find previews) ...
     let mut root_to_preview_map: HashMap<usize, String> = HashMap::new();
     for root_index in likely_root_indices.iter() {
          if let Some(root_entry) = entries.get(*root_index) {
              let root_prefix = if root_entry.path.ends_with('/') { root_entry.path.clone() } else { format!("{}/", root_entry.path) };
              for candidate in preview_candidates.iter() {
                  let potential_preview_path = format!("{}{}", root_prefix, candidate);
                  if entries.iter().any(|e| !e.is_dir && e.path.eq_ignore_ascii_case(&potential_preview_path)) {
                      root_to_preview_map.insert(*root_index, potential_preview_path);
                      break;
                  }
              }
          }
     }
    // ... (Pass 4: Deduction) ...
    let mut deduced_mod_name: Option<String> = None;
    let mut deduced_author: Option<String> = None;
    // Initialize final deduced slugs
    let mut final_deduced_category_slug: Option<String> = None;
    let mut final_deduced_entity_slug: Option<String> = None;
    // Raw hints extracted from INI
    let mut raw_ini_type_found: Option<String> = None;
    let mut raw_ini_target_found: Option<String> = None;
    // Preview path detected within archive
    let mut detected_preview_internal_path : Option<String> = None;
    let mut first_likely_root_processed = false;

    // --- 1. Deduce from INI in First Likely Root ---
    println!("[analyze_archive] Starting Pass 4: Deduction...");
    for (index, entry) in entries.iter_mut().enumerate() {
        if likely_root_indices.contains(&index) {
            entry.is_likely_mod_root = true; // Mark the entry
            println!("[analyze_archive] Found likely root: {}", entry.path);
            if !first_likely_root_processed {
                first_likely_root_processed = true;
                let root_prefix = if entry.path.ends_with('/') { entry.path.clone() } else { format!("{}/", entry.path) };
                // Find the first INI file *directly* inside this root
                if let Some((_ini_path, ini_content)) = ini_contents.iter().find(|(p, _)| p.starts_with(&root_prefix) && p.trim_start_matches(&root_prefix).find('/') == None) {
                    println!("[analyze_archive] Found INI in root {}: {}", root_prefix, _ini_path);
                    if let Ok(ini) = Ini::load_from_str(ini_content) {
                        // --- Temporary storage for extracted hints ---
                        let mut extracted_target: Option<String> = None;
                        let mut extracted_type: Option<String> = None;
                        // ---
                        for section_name in ["Mod", "Settings", "Info", "General"] {
                            if let Some(section) = ini.section(Some(section_name)) {
                                // Extract Name, Author
                                let name_val = section.get("Name").or_else(|| section.get("ModName"));
                                // Use the INI name if found, otherwise keep the initial filename guess
                                if let Some(name) = name_val {
                                    let cleaned_ini_name = MOD_NAME_CLEANUP_REGEX.replace_all(name, "").trim().to_string();
                                    if !cleaned_ini_name.is_empty() {
                                        deduced_mod_name = Some(cleaned_ini_name);
                                    }
                                }
                                let author_val = section.get("Author");
                                if author_val.is_some() { deduced_author = author_val.map(String::from); }

                                // Extract Raw Hints
                                let target_val = section.get("Target").or_else(|| section.get("Entity")).or_else(|| section.get("Character"));
                                if target_val.is_some() { extracted_target = target_val.map(|s| s.trim().to_string()); }
                                let type_val = section.get("Type").or_else(|| section.get("Category"));
                                if type_val.is_some() { extracted_type = type_val.map(|s| s.trim().to_string()); }
                            }
                        }
                        // Log extracted hints and assign to outer scope
                        println!("[analyze_archive] INI Extracted Hints: Target='{:?}', Type='{:?}'", extracted_target, extracted_type);
                        raw_ini_target_found = extracted_target;
                        raw_ini_type_found = extracted_type;
                    } else {
                        eprintln!("[analyze_archive] Warning: Failed to parse INI content from {}", _ini_path);
                    }
                } else {
                    println!("[analyze_archive] No INI found directly in root: {}", root_prefix);
                }

                // --- Try matching INI Target Hint (USE HELPER) ---
                if final_deduced_entity_slug.is_none() { // Only run if not already found
                    if let Some(target_hint) = &raw_ini_target_found {
                        println!("[analyze_archive] Trying INI target hint matching...");
                        // Use the reusable helper function
                        if let Some(slug) = find_entity_slug_from_hint(target_hint, &maps) {
                            final_deduced_entity_slug = Some(slug);
                            println!("[analyze_archive]   -> Found entity via INI target hint: '{}' -> {}", target_hint, final_deduced_entity_slug.as_ref().unwrap());
                        }
                    } else {
                        println!("[analyze_archive] No INI target hint found.");
                    }
                }

                // --- Try matching INI Type Hint (Category) ---
                if final_deduced_category_slug.is_none() { // Only run if not already found
                    if let Some(type_hint) = &raw_ini_type_found {
                        let lower_type_hint = type_hint.to_lowercase();
                        println!("[analyze_archive] Trying INI type hint: '{}' (lowercase: '{}')", type_hint, lower_type_hint);

                        // Prio 1: Exact slug
                        if maps.category_slug_to_id.contains_key(type_hint) {
                            final_deduced_category_slug = Some(type_hint.clone());
                            println!("[analyze_archive]   -> Matched category via INI exact slug: {}", type_hint);
                        }
                        // Prio 2: Exact lowercase name -> original slug
                        else if let Some(slug) = maps.lowercase_category_name_to_slug.get(&lower_type_hint) {
                            final_deduced_category_slug = Some(slug.clone());
                            println!("[analyze_archive]   -> Matched category via INI exact lowercase name: {} -> {}", lower_type_hint, slug);
                        }
                        // Prio 3: Known name starts with hint
                        else {
                            for (cat_name_lower, cat_slug) in &maps.lowercase_category_name_to_slug {
                                if cat_name_lower.starts_with(&lower_type_hint) {
                                    final_deduced_category_slug = Some(cat_slug.clone());
                                    println!("[analyze_archive]   -> Matched category via INI name prefix: '{}' starts with '{}' -> {}", cat_name_lower, lower_type_hint, cat_slug);
                                    break;
                                }
                            }
                        }
                        // Prio 4: Known name contains hint
                        if final_deduced_category_slug.is_none() {
                            for (cat_name_lower, cat_slug) in &maps.lowercase_category_name_to_slug {
                                if lower_type_hint.len() > 2 && cat_name_lower.contains(&lower_type_hint) {
                                    final_deduced_category_slug = Some(cat_slug.clone());
                                    println!("[analyze_archive]   -> Matched category via INI name contains: '{}' contains '{}' -> {}", cat_name_lower, lower_type_hint, cat_slug);
                                    break;
                                }
                            }
                        }
                        if final_deduced_category_slug.is_none() {
                            println!("[analyze_archive]   -> No category match found from INI type hint.");
                        }
                    } else {
                        println!("[analyze_archive] No INI type hint found.");
                    }
                }

                // Use detected preview if available for this root
                if let Some(preview_path) = root_to_preview_map.get(&index) {
                    detected_preview_internal_path = Some(preview_path.clone());
                    println!("[analyze_archive] Detected preview for this root: {}", preview_path);
                }

                // --- Break after processing the first root's INI ---
                println!("[analyze_archive] Finished processing first likely root INI.");
                break;
            }
        }
    }
    // --- End INI Deduction ---


    // --- 2. Deduce from Internal Filenames ---
    if final_deduced_entity_slug.is_none() {
        println!("[analyze_archive] Trying internal filename matching...");
        let mut file_match_found = false;
        // Iterate through ALL file entries in the archive
        for entry in &entries {
            if !entry.is_dir {
                // Get filename stem (without extension)
                let filename = entry.path.split('/').last().unwrap_or(&entry.path); // Get last component
                if let Some(stem) = Path::new(filename).file_stem().and_then(OsStr::to_str) {
                    if !stem.is_empty() {
                        // Use the helper to check if the stem matches an entity
                        if let Some(slug) = find_entity_slug_from_hint(stem, &maps) {
                            final_deduced_entity_slug = Some(slug);
                            println!("[analyze_archive]   -> Found entity via internal filename stem: '{}' -> {}", stem, final_deduced_entity_slug.as_ref().unwrap());
                            file_match_found = true;
                            break; // Found a match from a file, stop searching files
                        }
                    }
                }
            }
        }
        if !file_match_found {
            println!("[analyze_archive]   -> No entity match found from internal filenames.");
        }
    } else {
        println!("[analyze_archive] Skipping internal filename check (entity already found).")
    }
    // --- End Internal Filename Deduction ---


    // --- 3. Deduce from Archive Filename (USE HELPER - Lower Priority) ---
    if final_deduced_entity_slug.is_none() || final_deduced_category_slug.is_none() {
        println!("[analyze_archive] Attempting deduction from archive filename...");
        if let Some(stem) = file_path.file_stem().and_then(OsStr::to_str) {
            // Try matching stem against Entities (USE HELPER)
            if final_deduced_entity_slug.is_none() {
                println!("[analyze_archive] Trying archive filename stem for Entity: '{}'", stem);
                if let Some(slug) = find_entity_slug_from_hint(stem, &maps) {
                    final_deduced_entity_slug = Some(slug);
                    println!("[analyze_archive]   -> Found entity via filename.");
                } else {
                    println!("[analyze_archive]   -> No entity match found from filename.");
                }
            }

            // Try matching stem against Categories
            if final_deduced_category_slug.is_none() {
                let cleaned_stem = clean_and_extract_name(stem);
                println!("[analyze_archive] Trying archive filename stem for Category: '{}' (cleaned lowercase: '{}')", stem, cleaned_stem);

                // Prio 1: Exact slug (original stem)
                if maps.category_slug_to_id.contains_key(stem) {
                    final_deduced_category_slug = Some(stem.to_string());
                    println!("[analyze_archive]   -> Matched category from filename via exact slug: {}", stem);
                }
                // Prio 2: Exact lowercase cleaned name match -> original slug
                else if let Some(slug) = maps.lowercase_category_name_to_slug.get(&cleaned_stem) {
                    final_deduced_category_slug = Some(slug.clone());
                    println!("[analyze_archive]   -> Matched category from filename via exact cleaned name: {} -> {}", cleaned_stem, slug);
                }
                // Prio 3: Known name contains cleaned stem part
                else {
                    let stem_words: Vec<&str> = cleaned_stem.split_whitespace().filter(|w| w.len() > 2).collect();
                    if !stem_words.is_empty() {
                        'cat_loop: for (cat_name_lower, cat_slug) in &maps.lowercase_category_name_to_slug {
                            for word in &stem_words {
                                if cat_name_lower.contains(word) {
                                    final_deduced_category_slug = Some(cat_slug.clone());
                                    println!("[analyze_archive]   -> Matched category from filename via word contains: '{}' contains '{}' -> {}", cat_name_lower, word, cat_slug);
                                    break 'cat_loop;
                                }
                            }
                        }
                    }
                }
                if final_deduced_category_slug.is_none() {
                    println!("[analyze_archive]   -> No category match found from filename.");
                }
            }
        } else {
            println!("[analyze_archive] Could not get filename stem.");
        }
    } else {
        println!("[analyze_archive] Skipping filename deduction (already found entity and category).");
    }
    // --- End Filename Deduction ---


    // --- 4. Final Category Lookup (If needed) ---
    if final_deduced_entity_slug.is_some() && final_deduced_category_slug.is_none() {
        let entity_slug = final_deduced_entity_slug.as_ref().unwrap();
        println!("[analyze_archive] Entity slug '{}' found, but category slug is missing. Looking up category...", entity_slug);
        if let Some(cat_slug) = maps.entity_slug_to_category_slug.get(entity_slug) {
            final_deduced_category_slug = Some(cat_slug.clone());
            println!("[analyze_archive]   -> Found category slug '{}' from entity map.", cat_slug);
        } else {
            eprintln!("[analyze_archive]   -> Warning: Could not find category slug for deduced entity slug '{}' in maps!", entity_slug);
        }
    }
    // --- End Final Category Lookup ---


    // --- Fallback name deduction & final cleanup ---
    // Use cleaned archive name if INI name wasn't found or was empty after cleaning
    if deduced_mod_name.is_none() || deduced_mod_name.as_deref() == Some("") {
        deduced_mod_name = file_path.file_stem()
            .and_then(OsStr::to_str)
            .map(|s| clean_and_extract_name(s)); // Use cleaner here too
        println!("[analyze_archive] Used archive filename for deduced name: {:?}", deduced_mod_name);
    }
    // Final cleanup on whatever name we ended up with
    if let Some(name) = &deduced_mod_name {
        let cleaned = clean_and_extract_name(name); // Use cleaner
        if !cleaned.is_empty() {
            // If cleaning didn't result in empty, use the cleaned version
            deduced_mod_name = Some(cleaned);
        } else {
            // If cleaning resulted in empty, revert to original file stem as last resort
            deduced_mod_name = file_path.file_stem().and_then(OsStr::to_str).map(String::from);
            println!("[analyze_archive] Warning: Name cleanup resulted in empty string, using raw file stem: {:?}", deduced_mod_name);
        }
    }


    // --- Final Log ---
    println!("[analyze_archive] Final Deductions: Name={:?}, Author={:?}, Category={:?}, Entity={:?}, Preview={:?}, RawINI Target={:?}, RawINI Type={:?}",
        deduced_mod_name, deduced_author, final_deduced_category_slug, final_deduced_entity_slug, detected_preview_internal_path, raw_ini_target_found, raw_ini_type_found);

    // --- Return Result ---
    Ok(ArchiveAnalysisResult {
        file_path: file_path_str,
        entries,
        deduced_mod_name,
        deduced_author,
        deduced_category_slug: final_deduced_category_slug,
        deduced_entity_slug: final_deduced_entity_slug,
        raw_ini_type: raw_ini_type_found,
        raw_ini_target: raw_ini_target_found,
        detected_preview_internal_path,
    })
}

#[command]
fn read_archive_file_content(archive_path_str: String, internal_file_path: String) -> CmdResult<Vec<u8>> {
    println!("[read_archive_file_content] Reading '{}' from archive '{}'", internal_file_path, archive_path_str);
    let archive_path = PathBuf::from(&archive_path_str);
    if !archive_path.is_file() { return Err(format!("Archive file not found: {}", archive_path.display())); }

    let extension = archive_path.extension().and_then(|os| os.to_str()).map(|s| s.to_lowercase());
    let internal_path_normalized = internal_file_path.replace("\\", "/");

    match extension.as_deref() {
        Some("zip") => {
            let file = fs::File::open(&archive_path).map_err(|e| format!("Zip Read: Failed open: {}", e))?;
            let mut archive = ZipArchive::new(file).map_err(|e| format!("Zip Read: Failed read archive: {}", e))?;

            // --- FIX: Assign match result to variable and return it ---
            let result = match archive.by_name(&internal_path_normalized) {
                Ok(mut file_in_zip) => {
                    let mut buffer = Vec::with_capacity(file_in_zip.size() as usize);
                    match file_in_zip.read_to_end(&mut buffer) {
                        Ok(_) => Ok(buffer), // Successful read
                        Err(e) => Err(format!("Zip Read: Failed read content: {}", e)),
                    }
                },
                Err(ZipError::FileNotFound) => Err(format!("Zip Read: Internal file '{}' not found.", internal_file_path)),
                Err(e) => Err(format!("Zip Read: Error accessing internal file '{}': {}", internal_file_path, e)),
            };
            result // Return the result stored in the variable
            // --- END FIX ---
        }
        Some("7z") => {
            // --- 7z logic remains the same as previously corrected ---
            let mut found_content: Option<Vec<u8>> = None;
            let mut found_error: Option<String> = None;
            let mut archive = sevenz_rust::SevenZReader::open(&archive_path_str, Password::empty())
                .map_err(|e| format!("7z Read: Failed open: {}", e))?;

            archive.for_each_entries(|entry, reader| {
                if found_content.is_some() || found_error.is_some() { return Ok(false); }
                let entry_name_normalized = entry.name().replace("\\", "/");
                if entry_name_normalized == internal_path_normalized {
                    let mut content_bytes = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let read_result: Result<(), io::Error> = (|| { // Use closure for ? propagation
                        loop {
                            let bytes_read = reader.read(&mut buffer)?;
                            if bytes_read == 0 { break; }
                            content_bytes.extend_from_slice(&buffer[..bytes_read]);
                        }
                        Ok(())
                    })(); // Immediately invoke

                    match read_result {
                        Ok(()) => found_content = Some(content_bytes),
                        Err(e) => found_error = Some(format!("7z Read: Error reading content '{}': {}", internal_file_path, e)),
                    }
                    return Ok(false); // Stop processing after finding (or failing to read) the file
                }
                Ok(true)
            })
            .map_err(|e: sevenz_rust::Error| format!("7z Read: Error iterating entries: {}", e))?;

            if let Some(content) = found_content { Ok(content) }
            else if let Some(err) = found_error { Err(err) }
            else { Err(format!("7z Read: Internal file '{}' not found.", internal_file_path)) }
        }
        Some("rar") => {
            let mut archive = Archive::new(&archive_path_str)
                .open_for_processing() // Need Process mode to read content
                .map_err(|e| e.to_string())?;
            let mut found_content: Option<Vec<u8>> = None;

            loop {
                match archive.read_header() {
                    Ok(Some(header_state)) => { // Returns OpenArchive<..., CursorBeforeFile>
                        let entry_filename = &header_state.entry().filename; // Access header via entry()
                        let entry_name_normalized = entry_filename.to_string_lossy().replace("\\", "/");

                        if entry_name_normalized == internal_path_normalized {
                            // Found the file, process it using read()
                            match header_state.read() {
                                Ok((bytes, _next_archive_state)) => { // Successfully read
                                    found_content = Some(bytes);
                                    break; // Found and read, exit loop
                                }
                                Err(e) => { // Error during reading
                                    return Err(format!("Rar Read: Error reading content '{}': {}", internal_file_path, e));
                                }
                            }
                        } else {
                            // Not the file we want, skip it
                            archive = header_state.skip().map_err(|e| e.to_string())?; // Skip and update archive state
                        }
                    }
                    Ok(None) => break, // End of archive, file not found
                    Err(e) => return Err(format!("Rar Read: Error reading header: {}", e)),
                }
            }
            found_content.ok_or_else(|| format!("Rar Read: Internal file '{}' not found.", internal_file_path))
        }
        _ => Err(format!("Unsupported archive type for reading: {:?}", extension)),
    }
}

#[command]
fn import_archive(
    archive_path_str: String,
    target_entity_slug: String,
    selected_internal_root: String, // Frontend still provides this, empty means "extract all"
    mod_name: String,
    description: Option<String>,
    author: Option<String>,
    category_tag: Option<String>,
    image_data: Option<Vec<u8>>,
    selected_preview_absolute_path: Option<String>,
    preset_ids: Option<Vec<i64>>,
    db_state: State<DbState>
) -> CmdResult<()> {
    println!("[import_archive] Importing '{}', internal path '{}' for entity '{}'. Image Data Provided: {}. Add to presets: {:?}",
        archive_path_str,
        if selected_internal_root.is_empty() { "(Extract All)" } else { &selected_internal_root }, // Indicate if extracting all
        target_entity_slug,
        image_data.is_some(),
        preset_ids);

    // --- Basic Validation & Setup ---
    if mod_name.trim().is_empty() { return Err("Mod Name cannot be empty.".to_string()); }
    if target_entity_slug.trim().is_empty() { return Err("Target Entity must be selected.".to_string()); }
    let archive_path = PathBuf::from(&archive_path_str);
    if !archive_path.is_file() { return Err(format!("Archive file not found: {}", archive_path.display())); }

    let mut conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;

    let base_mods_path_str = get_setting_value(&conn_guard, SETTINGS_KEY_MODS_FOLDER)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Mods folder path not set".to_string())?;
    let base_mods_path = PathBuf::from(base_mods_path_str);

    let (target_category_slug, target_entity_id): (String, i64) = conn_guard.query_row(
        "SELECT c.slug, e.id FROM entities e JOIN categories c ON e.category_id = c.id WHERE e.slug = ?1",
        params![target_entity_slug], |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => format!("Target entity '{}' not found.", target_entity_slug),
        _ => format!("DB Error get target entity: {}", e)
    })?;

    let target_mod_folder_name = mod_name.trim().replace(" ", "_").replace(".", "_").replace("'", "").replace("\"", "");
    if target_mod_folder_name.is_empty() { return Err("Mod Name results in invalid folder name.".to_string()); }
    let final_mod_dest_path = base_mods_path.join(&target_category_slug).join(&target_entity_slug).join(&target_mod_folder_name);

    fs::create_dir_all(&final_mod_dest_path)
        .map_err(|e| format!("Failed create dest directory '{}': {}", final_mod_dest_path.display(), e))?;
    println!("[import_archive] Target destination folder created/ensured: {}", final_mod_dest_path.display());

    let tx = conn_guard.transaction().map_err(|e| format!("Failed start import transaction: {}", e))?;

    // --- Extraction Logic ---
    println!("[import_archive] Starting extraction...");
    let extension = archive_path.extension().and_then(|os| os.to_str()).map(|s| s.to_lowercase());
    // Normalize and prepare the prefix path IF a root was selected
    let prefix_to_extract_norm = selected_internal_root.replace("\\", "/");
    let prefix_to_extract = prefix_to_extract_norm.strip_suffix('/').unwrap_or(&prefix_to_extract_norm);
    let prefix_path = Path::new(prefix_to_extract);
    let extract_all = prefix_to_extract.is_empty(); // Flag to determine if extracting all
    println!("[import_archive] Extract All Mode: {}", extract_all);
    let mut files_extracted_count = 0;

    let extraction_result: Result<usize, String> = (|| {
        match extension.as_deref() {
        Some("zip") => {
             let file = fs::File::open(&archive_path).map_err(|e| format!("Zip Extract: Failed open: {}", e))?;
             let mut archive = ZipArchive::new(file).map_err(|e| format!("Zip Extract: Failed read archive: {}", e))?;
             for i in 0..archive.len() {
                  let mut file_in_zip = archive.by_index(i).map_err(|e| format!("Zip Extract: Failed read entry #{}: {}", i, e))?;
                  let internal_path_obj_opt = file_in_zip.enclosed_name().map(|p| p.to_path_buf());
                  if internal_path_obj_opt.is_none() { continue; }
                  let internal_path_obj = internal_path_obj_opt.unwrap();

                  let (should_extract, relative_path_to_dest_obj) = if extract_all {
                      // Extracting all: always extract, relative path is the full internal path
                      (true, internal_path_obj.clone()) // Clone needed as we check it later
                  } else {
                      // Specific root selected: check prefix
                      let should = internal_path_obj.starts_with(prefix_path);
                      let relative_path = if should {
                          // Strip the prefix if it matches
                          internal_path_obj.strip_prefix(prefix_path).map(|p| p.to_path_buf()).ok()
                      } else {
                          None
                      };
                      (should && relative_path.is_some(), relative_path.unwrap_or_default())
                  };

                  if !should_extract || relative_path_to_dest_obj.as_os_str().is_empty() { continue; }
                  let outpath = final_mod_dest_path.join(&relative_path_to_dest_obj);

                  if file_in_zip.is_dir() {
                      fs::create_dir_all(&outpath).map_err(|e| format!("Zip Extract: Failed create dir '{}': {}", outpath.display(), e))?;
                  } else {
                      if let Some(p) = outpath.parent() { if !p.exists() { fs::create_dir_all(&p).map_err(|e| format!("Zip Extract: Failed create parent '{}': {}", p.display(), e))?; } }
                      let mut outfile = fs::File::create(&outpath).map_err(|e| format!("Zip Extract: Failed create file '{}': {}", outpath.display(), e))?;
                      std::io::copy(&mut file_in_zip, &mut outfile).map_err(|e| format!("Zip Extract: Failed copy content '{}': {}", outpath.display(), e))?;
                      files_extracted_count += 1;
                  }
             }
        }
        Some("7z") => {
            let mut archive = sevenz_rust::SevenZReader::open(&archive_path_str, Password::empty())
                .map_err(|e| format!("7z Extract: Failed open: {}", e))?;
             archive.for_each_entries(|entry, reader| {
                 let internal_path_str = entry.name().replace("\\", "/");
                 let internal_path_obj = PathBuf::from(&internal_path_str);

                 let (should_extract, relative_path_to_dest_obj) = if extract_all {
                      (true, internal_path_obj.clone())
                 } else {
                      let should = internal_path_obj.starts_with(prefix_path);
                      let relative_path = if should { internal_path_obj.strip_prefix(prefix_path).map(|p| p.to_path_buf()).ok() } else { None };
                      (should && relative_path.is_some(), relative_path.unwrap_or_default())
                 };
                 if !should_extract || relative_path_to_dest_obj.as_os_str().is_empty() { return Ok(true); } // Skip to next
                 let outpath = final_mod_dest_path.join(&relative_path_to_dest_obj);

                 if entry.is_directory() {
                    fs::create_dir_all(&outpath)?;
                 } else {
                    if let Some(p) = outpath.parent() { if !p.exists() { fs::create_dir_all(&p)?; }}
                    let mut outfile = fs::File::create(&outpath)?;
                    let mut buffer = [0u8; 4096];
                    loop {
                        let bytes_read = reader.read(&mut buffer)?;
                        if bytes_read == 0 { break; }
                        outfile.write_all(&buffer[..bytes_read])?;
                    }
                    files_extracted_count += 1;
                 }
                 Ok(true) // Continue to next entry
             })
             .map_err(|e: sevenz_rust::Error| format!("7z Extract: Error processing entries: {}", e))?;
        }
        Some("rar") => {
            let mut archive = Archive::new(&archive_path_str).open_for_processing()
                .map_err(|e| e.to_string())?;
            loop {
                match archive.read_header().map_err(|e| e.to_string())? {
                    Some(header_state) => {
                        let entry_filename = &header_state.entry().filename;
                        let internal_path_str = entry_filename.to_string_lossy().replace("\\", "/").to_string();
                        let internal_path_obj = PathBuf::from(&internal_path_str);

                        let (should_extract, relative_path_to_dest_obj) = if extract_all {
                            (true, internal_path_obj.clone())
                        } else {
                            let should = internal_path_obj.starts_with(prefix_path);
                            let relative_path = if should { internal_path_obj.strip_prefix(prefix_path).map(|p| p.to_path_buf()).ok() } else { None };
                            (should && relative_path.is_some(), relative_path.unwrap_or_default())
                        };
                        if !should_extract || relative_path_to_dest_obj.as_os_str().is_empty() {
                            archive = header_state.skip().map_err(|e| e.to_string())?;
                            continue; // Skip to next
                        }
                        let outpath = final_mod_dest_path.join(&relative_path_to_dest_obj);

                        if header_state.entry().is_directory() {
                            fs::create_dir_all(&outpath).map_err(|e| format!("Rar Extract: Failed create dir '{}': {}", outpath.display(), e))?;
                            archive = header_state.skip().map_err(|e| e.to_string())?;
                        } else {
                            if let Some(p) = outpath.parent() { if !p.exists() { fs::create_dir_all(&p).map_err(|e| format!("Rar Extract: Failed create parent '{}': {}", p.display(), e))?; }}
                            archive = header_state.extract_to(&outpath).map_err(|e| e.to_string())?;
                            files_extracted_count += 1;
                        }
                    }
                    None => break, // End of archive
                }
            }
        }
        _ => return Err(format!("Unsupported archive type for extraction: {:?}", extension)),
        }
        Ok(files_extracted_count) // Return count on success
    })();

    // Handle extraction result
    let files_extracted_count = extraction_result.map_err(|e| {
         fs::remove_dir_all(&final_mod_dest_path).ok();
         e
    })?;
    println!("[import_archive] Extracted {} files.", files_extracted_count);

    // --- Handle Preview Image ---
    let mut image_filename_for_db: Option<String> = None;
    if let Some(data) = image_data {
        println!("[import_archive] Handling provided image data ({} bytes)", data.len());
        let target_image_path = final_mod_dest_path.join(TARGET_IMAGE_FILENAME);
        match fs::write(&target_image_path, data) {
            Ok(_) => {
                println!("[import_archive] Image data written successfully to '{}'.", target_image_path.display());
                image_filename_for_db = Some(TARGET_IMAGE_FILENAME.to_string());
            }
            Err(e) => {
                eprintln!("[import_archive] ERROR: Failed to save pasted image data to '{}': {}. Preview will be missing.", target_image_path.display(), e);
            }
        }
    }
    else if let Some(user_preview_path_str) = selected_preview_absolute_path {
        println!("[import_archive] Handling selected image file path: {}", user_preview_path_str);
        let source_path = PathBuf::from(&user_preview_path_str);
        if source_path.is_file() {
            let target_image_path = final_mod_dest_path.join(TARGET_IMAGE_FILENAME);
            match fs::copy(&source_path, &target_image_path) {
                Ok(_) => {
                    println!("[import_archive] Image file copied successfully to '{}'.", target_image_path.display());
                    image_filename_for_db = Some(TARGET_IMAGE_FILENAME.to_string());
                }
                Err(e) => {
                    eprintln!("[import_archive] ERROR: Failed copy user preview to '{}': {}. Preview will be missing.", target_image_path.display(), e);
                }
            }
        } else {
             println!("[import_archive] Warning: Selected preview file '{}' not found, skipping.", user_preview_path_str);
        }
    }
    else {
        let potential_extracted_image_path = final_mod_dest_path.join(TARGET_IMAGE_FILENAME);
        if potential_extracted_image_path.is_file() {
            println!("[import_archive] Using extracted {} as preview.", TARGET_IMAGE_FILENAME);
            image_filename_for_db = Some(TARGET_IMAGE_FILENAME.to_string());
        } else {
             println!("[import_archive] No pasted, selected, or extracted preview found.");
        }
    }
    println!("[import_archive] Image handling complete. Filename to save in DB: {:?}", image_filename_for_db);

    // --- Add to Database ---
    let relative_path_for_db = Path::new(&target_category_slug).join(&target_entity_slug).join(&target_mod_folder_name);
    let relative_path_for_db_str = relative_path_for_db.to_string_lossy().replace("\\", "/");

    let check_existing: Option<i64> = tx.query_row(
        "SELECT id FROM assets WHERE entity_id = ?1 AND folder_name = ?2",
        params![target_entity_id, relative_path_for_db_str], |row| row.get(0)
    ).optional().map_err(|e| format!("DB error check existing import '{}': {}", relative_path_for_db_str, e))?;

    if check_existing.is_some() {
        fs::remove_dir_all(&final_mod_dest_path).ok();
        return Err(format!("Database entry already exists for '{}'. Aborting.", relative_path_for_db_str));
    }

    println!("[import_archive] Adding asset to DB: entity_id={}, name={}, path={}, image={:?}", target_entity_id, mod_name, relative_path_for_db_str, image_filename_for_db);
    tx.execute(
        "INSERT INTO assets (entity_id, name, description, folder_name, image_filename, author, category_tag) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            target_entity_id, mod_name.trim(),
            description, relative_path_for_db_str,
            image_filename_for_db, author, category_tag
        ]
    ).map_err(|e| {
        fs::remove_dir_all(&final_mod_dest_path).ok();
        format!("Failed add imported mod to database: {}", e)
    })?;

    let new_asset_id = tx.last_insert_rowid();
    println!("[import_archive] Asset inserted with ID: {}", new_asset_id);

    // --- Add to Presets ---
    if let Some(ids) = preset_ids {
        if !ids.is_empty() {
            println!("[import_archive] Adding new asset {} to presets: {:?}", new_asset_id, ids);
            let mut insert_preset_stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO preset_assets (preset_id, asset_id, is_enabled) VALUES (?1, ?2, ?3)"
            ).map_err(|e| format!("Failed prepare preset asset insert: {}", e))?;
            for preset_id in ids {
                 insert_preset_stmt.execute(params![preset_id, new_asset_id, 1]) // Default to enabled state 1 when importing
                    .map_err(|e| format!("Failed insert new asset {} into preset {}: {}", new_asset_id, preset_id, e))?;
            }
            println!("[import_archive] Finished adding asset {} to presets.", new_asset_id);
        }
    }

    // --- Commit Transaction ---
    tx.commit().map_err(|e| {
        fs::remove_dir_all(&final_mod_dest_path).ok();
        format!("Failed to commit import transaction: {}", e)
    })?;

   println!("[import_archive] Import successful for '{}'", mod_name);
   Ok(())
}

#[command]
fn create_preset(name: String, db_state: State<DbState>) -> CmdResult<Preset> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Preset name cannot be empty.".to_string());
    }
    println!("[create_preset] Attempting to create preset: '{}'", name);

    let base_mods_path = get_mods_base_path_from_settings(&db_state)
        .map_err(|e| format!("Cannot create preset: {}", e))?;

    let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let mut conn = conn_guard;

    // Use a block scope for the transaction
    let preset_id = { // Start block scope for tx
        let tx = conn.transaction().map_err(|e| format!("Failed to start transaction: {}", e))?;

        // Check if name exists
        let existing_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM presets WHERE LOWER(name) = LOWER(?1)",
            params![name],
            |row| row.get(0),
        ).map_err(|e| format!("DB error checking preset name: {}", e))?;

        if existing_count > 0 {
            // Rollback happens automatically when tx is dropped on error return
            return Err(format!("Preset name '{}' already exists.", name));
        }

        // Insert new preset
        tx.execute("INSERT INTO presets (name) VALUES (?1)", params![name])
            .map_err(|e| format!("Failed to insert preset: {}", e))?;
        let new_preset_id = tx.last_insert_rowid();
        println!("[create_preset] Inserted preset with ID: {}", new_preset_id);

        // Use another block scope for the statement and iteration
        { // Start block scope for stmt
            let mut stmt = tx.prepare("SELECT id, folder_name FROM assets")
                .map_err(|e| format!("Failed to prepare asset fetch: {}", e))?;
            let asset_iter_result = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?.replace("\\", "/"),
                ))
            });

            match asset_iter_result {
                Ok(asset_iter) => {
                    for asset_result in asset_iter {
                        match asset_result {
                            Ok((asset_id, clean_relative_path_str)) => {
                                let clean_relative_path = PathBuf::from(&clean_relative_path_str);
                                let filename_osstr = clean_relative_path.file_name().unwrap_or_default();
                                let filename_str = filename_osstr.to_string_lossy();
                                if filename_str.is_empty() { continue; }

                                let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
                                let relative_parent_path = clean_relative_path.parent();

                                let full_path_if_enabled = base_mods_path.join(&clean_relative_path);
                                let full_path_if_disabled = match relative_parent_path {
                                    Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
                                    _ => base_mods_path.join(&disabled_filename),
                                };

                                let is_currently_enabled = if full_path_if_enabled.is_dir() { 1 }
                                                            else if full_path_if_disabled.is_dir() { 0 }
                                                            else {
                                                                println!("[create_preset] Warning: Asset ID {} folder not found on disk during preset save (path: {}). Skipping.", asset_id, clean_relative_path_str);
                                                                continue;
                                                            };

                                tx.execute(
                                    "INSERT INTO preset_assets (preset_id, asset_id, is_enabled) VALUES (?1, ?2, ?3)",
                                    params![new_preset_id, asset_id, is_currently_enabled],
                                ).map_err(|e| format!("Failed to save state for asset {}: {}", asset_id, e))?;
                            }
                            Err(e) => return Err(format!("Error fetching asset row: {}", e)), // Rollbacks on return
                        }
                    }
                }
                Err(e) => return Err(format!("Error preparing asset iterator: {}", e)), // Rollbacks on return
            }
        } // End block scope for stmt - stmt is dropped here, releasing borrow on tx

        // Commit the transaction
        tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;

        new_preset_id // Return the ID from the block
    }; // End block scope for tx

    println!("[create_preset] Preset '{}' created successfully.", name);

    Ok(Preset { id: preset_id, name: name.to_string(), is_favorite: false })
}


#[command]
fn get_presets(db_state: State<DbState>) -> CmdResult<Vec<Preset>> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let mut stmt = conn.prepare("SELECT id, name, is_favorite FROM presets ORDER BY name ASC")
        .map_err(|e| e.to_string())?;
    let preset_iter = stmt.query_map([], |row| {
        Ok(Preset {
            id: row.get(0)?,
            name: row.get(1)?,
            is_favorite: row.get::<_, i64>(2)? == 1,
        })
    }).map_err(|e| e.to_string())?;
    preset_iter.collect::<SqlResult<Vec<Preset>>>().map_err(|e| e.to_string())
}

#[command]
fn get_favorite_presets(db_state: State<DbState>) -> CmdResult<Vec<Preset>> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let mut stmt = conn.prepare(
        "SELECT id, name, is_favorite FROM presets WHERE is_favorite = 1 ORDER BY name ASC LIMIT 3"
    ).map_err(|e| e.to_string())?;
    let preset_iter = stmt.query_map([], |row| {
        Ok(Preset {
            id: row.get(0)?,
            name: row.get(1)?,
            is_favorite: row.get::<_, i64>(2)? == 1,
        })
    }).map_err(|e| e.to_string())?;
    preset_iter.collect::<SqlResult<Vec<Preset>>>().map_err(|e| e.to_string())
}

#[command]
async fn apply_preset(preset_id: i64, db_state: State<'_, DbState>, app_handle: AppHandle) -> CmdResult<()> {
    println!("[apply_preset] Applying preset ID: {}", preset_id);

    // Clone app_handle for potential use in error emission later
    let app_handle_clone = app_handle.clone();

    // --- Get base path first ---
    let base_mods_path = get_mods_base_path_from_settings(&db_state)
        .map_err(|e| format!("Cannot apply preset: {}", e))?;

    // --- Fetch preset assets ---
    let preset_assets_to_apply = { // Use block scope for connection lock
        let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
        let mut stmt = conn.prepare(
            "SELECT pa.asset_id, pa.is_enabled, a.folder_name, a.name
             FROM preset_assets pa
             JOIN assets a ON pa.asset_id = a.id
             WHERE pa.preset_id = ?1"
        ).map_err(|e| format!("Failed to prepare fetch for preset assets: {}", e))?;

        let preset_assets_iter_result = stmt.query_map(params![preset_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,                   // asset_id
                row.get::<_, i64>(1)? == 1,              // desired_is_enabled (bool)
                row.get::<_, String>(2)?.replace("\\", "/"), // clean_relative_path
                row.get::<_, String>(3)?,               // asset_name
            ))
        });

        match preset_assets_iter_result {
             Ok(iter) => iter.collect::<SqlResult<Vec<(i64, bool, String, String)>>>() // Include name
                              .map_err(|e| format!("Failed to collect preset assets: {}", e))?,
             Err(e) => return Err(format!("Error preparing preset asset iterator: {}", e)),
        }
    }; // Connection lock released here

    let total_assets = preset_assets_to_apply.len();
    println!("[apply_preset] Found {} assets in preset.", total_assets);

    // --- Emit START event ---
    app_handle.emit_all(PRESET_APPLY_START_EVENT, total_assets).ok();

    let mut processed_count = 0;
    let mut errors = Vec::new();

    for (asset_id, desired_is_enabled, clean_relative_path_str, asset_name) in preset_assets_to_apply {
        processed_count += 1;

        // --- Emit PROGRESS event ---
        let progress_message = format!("Processing: {} ({}/{})", asset_name, processed_count, total_assets);
        app_handle.emit_all(PRESET_APPLY_PROGRESS_EVENT, &ApplyProgress {
            processed: processed_count,
            total: total_assets,
            current_asset_id: Some(asset_id),
            message: progress_message.clone(),
        }).ok();
        println!("[apply_preset] {}", progress_message); // Also log to console

        // --- Filesystem logic ---
        let clean_relative_path = PathBuf::from(&clean_relative_path_str);
        let filename_osstr = clean_relative_path.file_name().unwrap_or_default();
        let filename_str = filename_osstr.to_string_lossy();
        if filename_str.is_empty() {
            let err_msg = format!("Skipping asset ID {}: Invalid folder name '{}'.", asset_id, clean_relative_path_str);
            println!("[apply_preset] {}", err_msg);
            errors.push(err_msg);
            continue;
        }

        let enabled_filename = filename_str.to_string();
        let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
        let relative_parent_path = clean_relative_path.parent();

        let construct_full_path = |name: &str| -> PathBuf {
            match relative_parent_path {
                Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(name),
                _ => base_mods_path.join(name),
            }
        };

        let full_path_if_enabled = construct_full_path(&enabled_filename);
        let full_path_if_disabled = construct_full_path(&disabled_filename);

        let current_path_on_disk: Option<PathBuf>;
        let current_is_enabled: bool;

        if full_path_if_enabled.is_dir() {
            current_path_on_disk = Some(full_path_if_enabled);
            current_is_enabled = true;
        } else if full_path_if_disabled.is_dir() {
            current_path_on_disk = Some(full_path_if_disabled);
            current_is_enabled = false;
        } else {
            let err_msg = format!("Skipping asset '{}' (ID {}): Folder not found on disk (path: '{}').", asset_name, asset_id, clean_relative_path_str);
            println!("[apply_preset] {}", err_msg);
            errors.push(err_msg);
            continue;
        }

        if current_is_enabled != desired_is_enabled {
            let target_path = if desired_is_enabled {
                construct_full_path(&enabled_filename)
            } else {
                construct_full_path(&disabled_filename)
            };
            let source_path = current_path_on_disk.unwrap();
            println!("[apply_preset] Renaming '{}' -> '{}' (Desired Enabled: {})", source_path.display(), target_path.display(), desired_is_enabled);
            match fs::rename(&source_path, &target_path) {
                Ok(_) => { /* Success */ }
                Err(e) => {
                     let err_msg = format!("Failed to rename asset '{}' (ID {}): {}", asset_name, asset_id, e);
                     println!("[apply_preset] Error: {}", err_msg);
                     errors.push(err_msg);
                }
            }
        }
        // Optional: Short delay for UI updates if needed
        // tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    } // End loop

    println!("[apply_preset] Finished applying preset ID {}. Errors: {}", preset_id, errors.len());

    if errors.is_empty() {
        // --- Emit COMPLETE event ---
        let summary = format!("Successfully applied preset ({} mods processed).", total_assets);
        app_handle.emit_all(PRESET_APPLY_COMPLETE_EVENT, &summary).ok();
        Ok(())
    } else {
        // --- Emit ERROR event ---
        let combined_errors = errors.join("\n");
        let error_summary = format!("Preset application completed with {} error(s).", errors.len());
        // You might want to send the full errors separately or just the summary
        app_handle_clone.emit_all(PRESET_APPLY_ERROR_EVENT, &error_summary).ok();
        Err(format!("{}\nDetails:\n{}", error_summary, combined_errors)) // Return error details too
    }
}


#[command]
fn toggle_preset_favorite(preset_id: i64, is_favorite: bool, db_state: State<DbState>) -> CmdResult<()> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let fav_value = if is_favorite { 1 } else { 0 };
    conn.execute(
        "UPDATE presets SET is_favorite = ?1 WHERE id = ?2",
        params![fav_value, preset_id],
    )
    .map_err(|e| format!("Failed to update favorite status: {}", e))?;
    Ok(())
}

#[command]
fn delete_preset(preset_id: i64, db_state: State<DbState>) -> CmdResult<()> {
    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    // Foreign key cascade should delete from preset_assets automatically
    let changes = conn.execute("DELETE FROM presets WHERE id = ?1", params![preset_id])
                      .map_err(|e| format!("Failed to delete preset: {}", e))?;
    if changes == 0 {
        Err(format!("Preset with ID {} not found.", preset_id))
    } else {
        Ok(())
    }
}

// --- Command to get Dashboard Stats ---
#[command]
fn get_dashboard_stats(db_state: State<DbState>) -> CmdResult<DashboardStats> {
    let base_mods_path = match get_mods_base_path_from_settings(&db_state) {
        Ok(p) => p,
        Err(_) => {
             // If base path isn't set, return default zeroed stats
            return Ok(DashboardStats {
                total_mods: 0,
                enabled_mods: 0,
                disabled_mods: 0,
                uncategorized_mods: 0,
                category_counts: HashMap::new(),
            });
        }
    };

    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;

    // 1. Total Mods
    let total_mods = conn.query_row("SELECT COUNT(*) FROM assets", [], |row| row.get::<_, i64>(0))
                         .map_err(|e| format!("Failed to get total mod count: {}", e))?;

    // 2. Uncategorized Mods
    let uncategorized_mods = conn.query_row(
        "SELECT COUNT(a.id) FROM assets a JOIN entities e ON a.entity_id = e.id WHERE e.slug LIKE '%-other'",
        [],
        |row| row.get::<_, i64>(0)
    ).map_err(|e| format!("Failed to get uncategorized mod count: {}", e))?;

    // 3. Category Counts
    let mut category_counts = HashMap::new();
    let mut cat_stmt = conn.prepare(
        "SELECT c.name, COUNT(a.id)
         FROM categories c
         JOIN entities e ON c.id = e.category_id
         JOIN assets a ON e.id = a.entity_id
         GROUP BY c.name
         HAVING COUNT(a.id) > 0" // Only include categories with mods
    ).map_err(|e| format!("Failed to prepare category count query: {}", e))?;

    let cat_rows = cat_stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }).map_err(|e| format!("Failed to execute category count query: {}", e))?;

    for row_result in cat_rows {
        match row_result {
            Ok((name, count)) => { category_counts.insert(name, count); }
            Err(e) => { eprintln!("[get_dashboard_stats] Error processing category count row: {}", e); }
        }
    }

    // 4. Enabled/Disabled Count (Disk Check)
    let mut enabled_mods = 0;
    let mut disabled_mods = 0;
    let mut disk_check_errors = 0;

    // Fetch folder names for checking
    let mut asset_folders_stmt = conn.prepare("SELECT folder_name FROM assets")
        .map_err(|e| format!("Failed to prepare asset folder fetch: {}", e))?;
    let asset_folder_rows = asset_folders_stmt.query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("Failed to query asset folders: {}", e))?;

    for folder_result in asset_folder_rows {
        match folder_result {
            Ok(clean_relative_path_str) => {
                 let clean_relative_path = PathBuf::from(clean_relative_path_str.replace("\\", "/"));
                 let filename_osstr = clean_relative_path.file_name().unwrap_or_default();
                 let filename_str = filename_osstr.to_string_lossy();
                 if filename_str.is_empty() { continue; }

                 let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
                 let relative_parent_path = clean_relative_path.parent();

                 let full_path_if_enabled = base_mods_path.join(&clean_relative_path);
                 let full_path_if_disabled = match relative_parent_path {
                    Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
                    _ => base_mods_path.join(&disabled_filename),
                 };

                 if full_path_if_enabled.is_dir() {
                     enabled_mods += 1;
                 } else if full_path_if_disabled.is_dir() {
                     disabled_mods += 1;
                 } else {
                     // Folder not found in either state - might have been deleted since last scan
                     // We don't count it as enabled or disabled.
                     disk_check_errors += 1;
                 }
            }
            Err(e) => { eprintln!("[get_dashboard_stats] Error fetching asset folder row: {}", e); }
        }
    }

    Ok(DashboardStats {
        total_mods,
        enabled_mods,
        disabled_mods,
        uncategorized_mods,
        category_counts,
    })
}


// --- Command to get App Version ---
#[command]
fn get_app_version() -> String {
    // Read from environment variable set by build script/Cargo
    env!("CARGO_PKG_VERSION").to_string()
}

#[command]
fn get_entities_by_category_with_counts(category_slug: String, db_state: State<DbState>) -> CmdResult<Vec<EntityWithCounts>> {
    println!("[get_entities_with_counts] Fetching for category: {}", category_slug);

    let base_mods_path = match get_mods_base_path_from_settings(&db_state) {
        Ok(p) => p,
        Err(_) => {
            println!("[get_entities_with_counts] Mods folder not set. Returning empty list.");
            return Ok(Vec::new());
        }
    };

    let conn = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;

    // 1. Get Category ID
    let category_id: i64 = conn.query_row(
        "SELECT id FROM categories WHERE slug = ?1",
        params![category_slug],
        |row| row.get(0),
    ).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => format!("Category '{}' not found", category_slug),
        _ => format!("DB Error getting category ID: {}", e),
    })?;

    // 2. Get Entities for the Category
    let mut entity_stmt = conn.prepare(
         "SELECT e.id, e.category_id, e.name, e.slug, e.details, e.base_image
          FROM entities e
          WHERE e.category_id = ?1
          ORDER BY CASE WHEN e.slug LIKE '%-other' THEN 0 ELSE 1 END ASC, e.name ASC"
     ).map_err(|e| format!("Failed to prepare entity query: {}", e))?;

    let entity_rows_iter = entity_stmt.query_map(params![category_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    }).map_err(|e| format!("Failed to query entities: {}", e))?;

    let mut results: Vec<EntityWithCounts> = Vec::new();

    // *** FIX: Apply .map_err() to the prepare call ***
    let mut asset_folder_stmt = conn.prepare("SELECT folder_name FROM assets WHERE entity_id = ?1")
                                     .map_err(|e| format!("Failed to prepare asset folder query: {}", e))?; // Prepare asset query once

    for entity_result in entity_rows_iter {
        match entity_result {
            Ok((id, cat_id, name, slug, details, base_image)) => {
                // 3. For each entity, get its assets and check disk status
                let mut total_mods_for_entity = 0;
                let mut enabled_mods_for_entity = 0;

                // Map potential errors when querying assets for *this specific* entity
                let asset_folder_rows_result = asset_folder_stmt.query_map(params![id], |row| row.get::<_, String>(0));

                match asset_folder_rows_result {
                     Ok(rows) => {
                        for folder_result in rows {
                            match folder_result {
                                Ok(clean_relative_path_str) => {
                                    total_mods_for_entity += 1;

                                    let clean_relative_path = PathBuf::from(clean_relative_path_str.replace("\\", "/"));
                                    let filename_osstr = clean_relative_path.file_name().unwrap_or_default();
                                    let filename_str = filename_osstr.to_string_lossy();
                                    if filename_str.is_empty() { continue; }

                                    // Check only enabled state path
                                    let full_path_if_enabled = base_mods_path.join(&clean_relative_path);
                                    if full_path_if_enabled.is_dir() {
                                        enabled_mods_for_entity += 1;
                                    }
                                }
                                Err(e) => eprintln!("[get_entities_with_counts] Error fetching asset folder row for entity {}: {}", id, e),
                            }
                        }
                    }
                    // Log the error but don't stop the whole process for one entity's assets failing
                    Err(e) => eprintln!("[get_entities_with_counts] Error querying asset folders for entity {}: {}", id, e),
                }

                results.push(EntityWithCounts {
                    id,
                    category_id: cat_id,
                    name,
                    slug,
                    details,
                    base_image,
                    total_mods: total_mods_for_entity,
                    enabled_mods: enabled_mods_for_entity,
                });
            }
            Err(e) => eprintln!("[get_entities_with_counts] Error processing entity row: {}", e),
        }
    }

    println!("[get_entities_with_counts] Found {} entities with counts for category '{}'", results.len(), category_slug);
    Ok(results)
}

#[command]
fn overwrite_preset(preset_id: i64, db_state: State<DbState>) -> CmdResult<()> {
    println!("[overwrite_preset] Attempting to overwrite preset ID: {}", preset_id);

    let base_mods_path = get_mods_base_path_from_settings(&db_state)
        .map_err(|e| format!("Cannot overwrite preset (failed to get mods path): {}", e))?;

    let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
    let mut conn = conn_guard; // Get mutable access to the MutexGuard content

    // Use a transaction for atomicity
    let tx = conn.transaction().map_err(|e| format!("Failed to start transaction: {}", e))?;

    // 1. Delete existing asset states for this preset
    println!("[overwrite_preset] Deleting old asset states for preset {}", preset_id);
    let delete_count = tx.execute("DELETE FROM preset_assets WHERE preset_id = ?1", params![preset_id])
        .map_err(|e| format!("Failed to delete old preset asset states: {}", e))?;
    println!("[overwrite_preset] Deleted {} old entries.", delete_count);

    // 2. Fetch all current assets from the main assets table
    let mut assets_to_save = Vec::<(i64, String)>::new(); // (asset_id, clean_relative_path)
    { // Scope for the statement
        let mut stmt = tx.prepare("SELECT id, folder_name FROM assets")
           .map_err(|e| format!("Failed to prepare asset fetch statement: {}", e))?;
        let asset_iter = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
                             .map_err(|e| format!("Failed to create asset query iterator: {}", e))?;

        for row_result in asset_iter {
            match row_result {
                Ok((asset_id, folder_name)) => {
                    assets_to_save.push((asset_id, folder_name.replace("\\", "/")));
                }
                Err(e) => {
                    // Log error for the specific row but continue fetching others
                    eprintln!("[overwrite_preset] Error fetching asset row from DB: {}", e);
                    // Optionally rollback transaction and return error
                    // tx.rollback().ok(); // Attempt rollback // Don't rollback here, let the error propagate
                    // return Err(format!("Error fetching asset row from DB: {}", e)); // Let caller handle rollback on error
                }
            }
        }
    }
    println!("[overwrite_preset] Fetched {} assets to check for saving.", assets_to_save.len());


    // --- FIX: Add block scope for insert_stmt ---
    let mut saved_count = 0;
    let mut not_found_count = 0;
    { // Start scope for insert_stmt
        // 3. Iterate through fetched assets, check disk state, and insert into preset_assets
        let mut insert_stmt = tx.prepare(
            "INSERT INTO preset_assets (preset_id, asset_id, is_enabled) VALUES (?1, ?2, ?3)"
        ).map_err(|e| format!("Failed to prepare insert statement for preset assets: {}", e))?;


        for (asset_id, clean_relative_path_str) in assets_to_save {
            let clean_relative_path = PathBuf::from(&clean_relative_path_str);
            let filename_osstr = clean_relative_path.file_name().unwrap_or_default();
            let filename_str = filename_osstr.to_string_lossy();
            if filename_str.is_empty() { continue; }

            // Check enabled state on disk
            let full_path_if_enabled = base_mods_path.join(&clean_relative_path);
            let is_currently_enabled_on_disk = if full_path_if_enabled.is_dir() {
                1 // Enabled
            } else {
                // Check disabled state only to confirm it exists somewhere, otherwise skip saving
                let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
                let relative_parent_path = clean_relative_path.parent();
                let full_path_if_disabled = match relative_parent_path {
                    Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
                    _ => base_mods_path.join(&disabled_filename),
                };
                if full_path_if_disabled.is_dir() {
                    0 // Disabled
                } else {
                    // Folder not found in either state - skip saving its state for this preset
                    println!("[overwrite_preset] Warning: Asset ID {} folder not found on disk during preset save (path: {}). Skipping.", asset_id, clean_relative_path_str);
                    not_found_count += 1;
                    continue; // Skip to next asset
                }
            };

            // Insert the current state into the preset
            insert_stmt.execute(params![preset_id, asset_id, is_currently_enabled_on_disk])
                .map_err(|e| format!("Failed to save state for asset {}: {}", asset_id, e))?;
            saved_count += 1;
        }
    } // --- End scope for insert_stmt --- `insert_stmt` is dropped here, releasing the borrow on `tx`


    // 4. Commit the transaction (Now safe as insert_stmt is out of scope)
    tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;

    println!("[overwrite_preset] Preset ID {} overwritten successfully. Saved state for {} assets (skipped {} not found).", preset_id, saved_count, not_found_count);
    Ok(())
}

#[command]
fn get_ini_keybinds(asset_id: i64, db_state: State<DbState>) -> Result<Vec<KeybindInfo>, String> { // CmdResult is Result<T, String>
    println!("[get_ini_keybinds] COMMAND START for asset ID: {}", asset_id);

    let result: Result<Vec<KeybindInfo>, String> = (|| {
        println!("[get_ini_keybinds] Attempting to acquire DB lock...");
        let conn_guard = db_state.0.lock().map_err(|_| {
            eprintln!("[get_ini_keybinds] ERROR: DB lock poisoned!");
            "DB lock poisoned".to_string()
        })?;
        println!("[get_ini_keybinds] DB lock acquired.");
        let conn = &*conn_guard; // Dereference the guard to get the connection

        println!("[get_ini_keybinds] Attempting to get base mods path setting directly...");
        // --- FIX: Provide arguments to query_row ---
        let mods_folder_path_str_opt: Option<String> = conn.query_row(
            "SELECT value FROM settings WHERE key = ?1", // SQL query
            params![SETTINGS_KEY_MODS_FOLDER],          // Parameters
            |row| row.get(0),                           // Mapping closure
        ).optional().map_err(|e| format!("DB error fetching mods folder setting: {}", e))?;
        // --- End Fix ---

        let base_mods_path = mods_folder_path_str_opt
            .map(PathBuf::from)
            .ok_or_else(|| {
                eprintln!("[get_ini_keybinds] ERROR: Mods folder path not set in settings.");
                "Mods folder path not set".to_string()
            })?;
        println!("[get_ini_keybinds] Base mods path obtained directly: {}", base_mods_path.display());


        println!("[get_ini_keybinds] Calling find_asset_ini_paths...");
        let ini_paths = find_asset_ini_paths(conn, asset_id, &base_mods_path)
            .map_err(|e| {
                eprintln!("[get_ini_keybinds] ERROR from find_asset_ini_paths: {}", e);
                format!("Error finding INI paths: {}", e)
            })?;
        println!("[get_ini_keybinds] find_asset_ini_paths returned {} paths.", ini_paths.len());

        // --- Release the lock explicitly before file I/O ---
        drop(conn_guard);
        println!("[get_ini_keybinds] DB lock released before file parsing.");

        if ini_paths.is_empty() {
             println!("[get_ini_keybinds] No INI files found for asset ID {}", asset_id);
             return Ok(Vec::new()); // Return empty early if no INIs exist
        }

        let mut found_keybinds: Vec<KeybindInfo> = Vec::new();

        for ini_path in ini_paths {
            println!("[get_ini_keybinds] Parsing INI at: {}", ini_path.display());

            let file = match File::open(&ini_path) {
                 Ok(f) => f,
                 Err(e) => {
                     eprintln!("[get_ini_keybinds] ERROR: Failed to open INI file {}: {}. Skipping.", ini_path.display(), e);
                     continue; // Skip to the next file if this one can't be opened
                 }
            };
            let reader = BufReader::new(file);

            let mut current_section_title: Option<String> = None;
            let mut found_constants_tag = false; // Track if we are past '; Constants'

            for line_result in reader.lines() {
                let line_raw = match line_result {
                    Ok(l) => l,
                    Err(_) => {
                         println!("[get_ini_keybinds] Warning: Skipping unreadable line in {}", ini_path.display());
                         continue;
                    }
                };
                let line = line_raw.trim(); // Trimmed version for processing

                // Check for '; Constants' tag
                 if !found_constants_tag && line.starts_with(';') && line[1..].trim_start().to_lowercase().contains("constants") {
                    println!("[get_ini_keybinds] Found '; Constants' marker in {}", ini_path.display());
                    found_constants_tag = true;
                    continue; // Move to next line after finding the marker
                }

                // Only process sections/keys if constants tag was found
                if found_constants_tag {
                    if line.starts_with('[') && line.ends_with(']') {
                        let section_name = line[1..line.len()-1].trim().to_string();
                        // Check if it's a keybind section
                        if section_name.to_lowercase().starts_with("key") {
                            current_section_title = Some(section_name); // Store the title
                        } else {
                            current_section_title = None; // Reset title if not a keybind section
                        }
                    } else if current_section_title.is_some() && line.to_lowercase().starts_with("key") && line.contains('=') {
                         // Only process 'key =' lines if we are inside a valid [Key...] section *after* '; Constants'
                         if let Some(value_part) = line.splitn(2, '=').nth(1) {
                             let keybind_value = value_part.trim().to_string();
                             if !keybind_value.is_empty() {
                                  // .unwrap() is safe because we checked is_some()
                                  found_keybinds.push(KeybindInfo {
                                      title: current_section_title.clone().unwrap(),
                                      key: keybind_value,
                                  });
                             }
                         }
                    }
                }
            } // End line loop

             // Check if keybinds were found in *this* file (after Constants)
             if !found_keybinds.is_empty() {
                println!("[get_ini_keybinds] SUCCESS: Found {} keybinds (after Constants) in {}. Returning.", found_keybinds.len(), ini_path.display());
                return Ok(found_keybinds); // Found binds, return them
            } else {
                 println!("[get_ini_keybinds] No keybinds found (after Constants) in {}. Checking next file.", ini_path.display());
             }
        } // --- End loop through INI paths ---

        // If loop finishes, no keybinds were found in any file
        println!("[get_ini_keybinds] No keybinds found (after Constants) after checking all INI files for asset ID {}", asset_id);
        Ok(Vec::new()) // Return empty Vec<KeybindInfo>

    })(); // Execute the closure

    println!("[get_ini_keybinds] COMMAND END for asset ID: {}", asset_id);
    result // Return the result of the closure (Result<Vec<KeybindInfo>, String>)
}

#[command]
fn open_asset_folder(asset_id: i64, db_state: State<DbState>) -> CmdResult<()> {
    println!("[open_asset_folder] COMMAND START for asset ID: {}", asset_id);
    let result = (|| {
        // ... (Lock acquisition, base path fetch, asset info fetch - remain the same) ...
        println!("[open_asset_folder] Attempting to acquire DB lock...");
        let conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;
        println!("[open_asset_folder] DB lock acquired.");
        let conn = &*conn_guard;

        println!("[open_asset_folder] Attempting to get base mods path setting directly...");
        let mods_folder_path_str_opt: Option<String> = conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![SETTINGS_KEY_MODS_FOLDER],
            |row| row.get(0),
        ).optional().map_err(|e| format!("DB error fetching mods folder setting: {}", e))?;

        let base_mods_path = mods_folder_path_str_opt
            .map(PathBuf::from)
            .ok_or_else(|| "Mods folder path not set".to_string())?;
        println!("[open_asset_folder] Base path obtained directly: {}", base_mods_path.display());

        println!("[open_asset_folder] Getting asset location info...");
        let asset_info = get_asset_location_info(conn, asset_id)
         .map_err(|e| format!("Failed to get asset info for opening folder: {}", e))?;
         println!("[open_asset_folder] Asset info found: {:?}", asset_info);


        // --- Determine the actual mod folder path on disk ---
        // Ensure clean_relative_path uses OS-specific separators when joining with base_mods_path
        // PathBuf::join handles this, but let's be explicit if needed later.
        // For now, assume PathBuf construction is correct.
        let relative_path_buf = PathBuf::from(&asset_info.clean_relative_path.replace("/", std::path::MAIN_SEPARATOR_STR)); // Ensure DB path uses native sep before join? Less critical usually.

        let filename_osstr = relative_path_buf.file_name().ok_or_else(|| format!("Could not extract filename from DB path: {}", asset_info.clean_relative_path))?;
        let filename_str = filename_osstr.to_string_lossy();
        let disabled_filename = format!("{}{}", DISABLED_PREFIX, filename_str);
        let relative_parent_path = relative_path_buf.parent();

        let full_path_if_enabled = base_mods_path.join(&relative_path_buf);
        let full_path_if_disabled = match relative_parent_path {
            Some(parent) if parent.as_os_str().len() > 0 => base_mods_path.join(parent).join(&disabled_filename),
            _ => base_mods_path.join(&disabled_filename),
        };

        let mod_folder_path_on_disk = if full_path_if_enabled.is_dir() {
            Some(full_path_if_enabled)
        } else if full_path_if_disabled.is_dir() {
            Some(full_path_if_disabled)
        } else {
            None
        };

        drop(conn_guard);
        println!("[open_asset_folder] DB lock released.");

        match mod_folder_path_on_disk {
            Some(mod_path) => {
                println!("[open_asset_folder] Target mod folder: {}", mod_path.display());

                let command_name;
                let arg;

                // --- FIX: Ensure backslashes for Windows explorer ---
                if cfg!(target_os = "windows") {
                    command_name = "explorer";
                    // Convert to string and explicitly replace forward slashes
                    arg = mod_path.to_string_lossy().replace("/", "\\");
                } else if cfg!(target_os = "macos") {
                    command_name = "open";
                    arg = mod_path.to_str().ok_or("Invalid UTF-8 path string for macOS")?.to_string();
                } else {
                    command_name = "xdg-open";
                    arg = mod_path.to_str().ok_or("Invalid UTF-8 path string for Linux")?.to_string();
                }
                // --- End Fix ---

                println!("Executing: {} \"{}\"", command_name, arg);

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
            None => {
                 println!("[open_asset_folder] Mod folder not found on disk for asset ID {}", asset_id);
                 Err(format!("Mod folder not found for asset ID {}", asset_id))
            }
        }

    })(); // Execute closure

    println!("[open_asset_folder] COMMAND END for asset ID: {}", asset_id);
    result
}

#[command]
fn add_asset_to_presets(asset_id: i64, preset_ids: Vec<i64>, db_state: State<DbState>) -> CmdResult<()> {
    if preset_ids.is_empty() {
        return Ok(()); // Nothing to do
    }
    println!("[add_asset_to_presets] Adding/Updating asset ID {} in presets: {:?}", asset_id, preset_ids);

    let base_mods_path = get_mods_base_path_from_settings(&db_state)
        .map_err(|e| format!("Cannot add/update presets (failed to get mods path): {}", e))?;

    let mut conn_guard = db_state.0.lock().map_err(|_| "DB lock poisoned".to_string())?;

    // Use a transaction for atomicity
    let tx = conn_guard.transaction().map_err(|e| format!("Failed to start transaction: {}", e))?;

    // Get current enabled state *once* before the loop
    let current_is_enabled = match get_current_asset_enabled_state(&tx, asset_id, &base_mods_path) {
         Ok(enabled) => if enabled { 1 } else { 0 },
         Err(e) => {
             eprintln!("[add_asset_to_presets] Error getting current state for asset {}: {}. Aborting.", asset_id, e);
             return Err(format!("Failed to determine current enabled state for asset {}: {}", asset_id, e));
         }
    };

    println!("[add_asset_to_presets] Determined current enabled state for asset {} as: {}", asset_id, current_is_enabled);

    let mut changes_made = 0; // Tracks total rows affected (inserts + updates)
    { // Scope for the statement
        // --- *** THE FIX: Use INSERT OR REPLACE *** ---
        let mut upsert_stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO preset_assets (preset_id, asset_id, is_enabled) VALUES (?1, ?2, ?3)"
        ).map_err(|e| format!("Failed to prepare upsert statement: {}", e))?;
        // --- *** END FIX *** ---

        for preset_id in preset_ids {
            let changes = upsert_stmt.execute(params![preset_id, asset_id, current_is_enabled])
                .map_err(|e| format!("Failed to upsert asset {} into preset {}: {}", asset_id, preset_id, e))?;
            changes_made += changes;
        }
    }

    tx.commit().map_err(|e| format!("Failed to commit transaction: {}", e))?;

    println!("[add_asset_to_presets] Successfully added/updated asset {} in presets. Rows affected: {}", asset_id, changes_made);
    Ok(())
}

#[command]
fn get_available_games(app_handle: AppHandle) -> CmdResult<Vec<String>> {
    let data_dir = get_app_data_dir(&app_handle).map_err(|e| e.to_string())?;

    let mut games: HashSet<String> = PREDEFINED_GAMES.iter().map(|&s| s.to_string()).collect();

    if data_dir.is_dir() {
        match fs::read_dir(data_dir) {
            Ok(entries) => {
                for entry_result in entries {
                    if let Ok(entry) = entry_result {
                        let path = entry.path();
                        if path.is_file() {
                             if let Some(filename_str) = path.file_name().and_then(|n| n.to_str()) {
                                // Check for archived DB files (e.g., app_data_genshin.sqlite)
                                if filename_str.starts_with(DB_FILENAME_PREFIX) && filename_str.ends_with(".sqlite") {
                                    let game_slug = filename_str
                                        .trim_start_matches(DB_FILENAME_PREFIX)
                                        .trim_end_matches(".sqlite");
                                    if !game_slug.is_empty() {
                                        games.insert(game_slug.to_string()); // Add discovered games
                                    }
                                }
                             }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: Could not read app data directory to find existing game DBs: {}", e);
            }
        }
    }

    let mut sorted_games: Vec<String> = games.into_iter().collect();
    sorted_games.sort(); // Sort alphabetically
    println!("Available games: {:?}", sorted_games); // Log the final list
    Ok(sorted_games)
}

#[command]
fn get_active_game(app_handle: AppHandle) -> CmdResult<String> {
    read_app_config(&app_handle)
        .map(|config| config.requested_active_game) // Return the requested game
        .map_err(|e| e.to_string())
}

#[command]
fn switch_game(app_handle: AppHandle, target_game_slug: String) -> CmdResult<String> { // Keep AppHandle for potential future use, though not needed for exit
    println!("Requesting switch to game config: {}", target_game_slug);

    let mut config = read_app_config(&app_handle).map_err(|e| e.to_string())?;
    let current_game_slug = config.requested_active_game.clone(); // Clone needed if used after config update

    if current_game_slug == target_game_slug {
        println!("Already requested game: {}. No change needed.", target_game_slug);
        // Return a different message if no action needed
        return Ok("Game already selected. No action taken.".to_string());
    }

    // Update only the requested game field
    config.requested_active_game = target_game_slug.clone();

    // Write the updated config back
    if let Err(e) = write_app_config(&app_handle, &config) {
        let err_msg = format!("CRITICAL: Failed to update app config with requested game: {}", e);
        eprintln!("{}", err_msg);
        // Don't exit here, let the user know the config failed
        return Err(err_msg);
    }
    println!("App config updated. Requested game: {}.", target_game_slug);

    // --- Removed app_handle.restart() ---

    // Return success message instructing manual restart
    Ok(format!("Successfully configured to switch to '{}' on next launch. Please close and restart the application.", target_game_slug.to_uppercase()))
}

#[command]
fn exit_app(app_handle: AppHandle) {
    println!("Received request to exit application.");
    // Exit the entire application process. The '0' is the exit code (0 usually means success).
    exit(0);
}

#[command]
fn run_traveler_migration(db_state: State<DbState>, app_handle: AppHandle) -> CmdResult<String> {
    // This command just calls the main logic function
    run_traveler_migration_logic(&db_state, &app_handle)
}

// --- Main Function ---
fn main() {
    let context = generate_context!(); // Generates context based on tauri.conf.json

    tauri::Builder::default()
        .setup(|app| {
            let app_handle = app.handle();
            println!("--- Application Setup Starting ---");

            let data_dir = match get_app_data_dir(&app_handle) {
                Ok(dir) => dir,
                Err(e) => {
                     // If we can't even determine the path, it's fatal.
                     eprintln!("FATAL: Cannot determine app data dir path: {}", e);
                     dialog::blocking::message(
                         app_handle.get_window("main").as_ref(),
                         "Fatal Error",
                         "Cannot determine the application data directory path."
                     );
                     std::process::exit(1);
                }
            };

            // Attempt to create the directory if it doesn't exist.
            if !data_dir.exists() {
                println!("App data directory does not exist, attempting to create: {}", data_dir.display());
                if let Err(e) = fs::create_dir_all(&data_dir) {
                    // If creation fails (permissions?), it's fatal.
                    eprintln!("FATAL: Failed to create app data directory at {}: {}", data_dir.display(), e);
                    dialog::blocking::message(
                        app_handle.get_window("main").as_ref(),
                        "Fatal Error",
                        &format!("Failed to create application data directory:\n{}\n\nPlease check permissions.", data_dir.display())
                    );
                    std::process::exit(1);
                }
                 println!("App data directory created successfully.");
            } else {
                println!("App data directory already exists: {}", data_dir.display());
            }

            // --- 1. Read Target Config ---
            // Reads app_config.json to determine the last known state and the user's requested state.
            let mut config = match read_app_config(&app_handle) {
                 Ok(cfg) => cfg,
                 Err(e) => {
                     // If config can't be read/created, the app cannot function correctly.
                     eprintln!("FATAL: Failed to read or create app config: {}", e);
                     // Show a blocking message to the user before exiting.
                     dialog::blocking::message(
                         app_handle.get_window("main").as_ref(), // Get main window handle if possible
                         "Fatal Configuration Error",
                         &format!("Could not read or create app configuration:\n{}", e)
                     );
                     std::process::exit(1); // Exit the application.
                 }
            };
            // Store the slugs from the config for easier access.
            let last_slug = &config.last_active_game;
            let requested_slug = &config.requested_active_game;
            println!("Config Read: Last Active='{}', Requested='{}'", last_slug, requested_slug);

            // --- 2. Perform Pre-Initialization DB Rename Logic ---
            // This block executes ONLY if the last known active game is different from the requested one.
            if last_slug != requested_slug {
                println!("Switch required: '{}' -> '{}'", last_slug, requested_slug);
                // Get the application's data directory path.
                let data_dir = match get_app_data_dir(&app_handle) {
                     Ok(dir) => dir,
                     Err(e) => {
                          // Cannot proceed without the data directory.
                          eprintln!("FATAL: Cannot get app data dir: {}", e);
                          dialog::blocking::message(
                              app_handle.get_window("main").as_ref(),
                              "Fatal Error",
                              "Cannot determine application data directory."
                          );
                          std::process::exit(1);
                     }
                };
                // Define paths for the active DB and the archive files for the last and requested games.
                let active_db_path = data_dir.join(ACTIVE_DB_FILENAME);
                let last_game_archive_path = data_dir.join(format!("{}{}.sqlite", DB_FILENAME_PREFIX, last_slug));
                let requested_game_archive_path = data_dir.join(format!("{}{}.sqlite", DB_FILENAME_PREFIX, requested_slug));

                // Step A: Archive the current active DB (if it exists).
                // This should correspond to the 'last_slug'.
                if active_db_path.exists() {
                    println!("Archiving '{}' (from '{}') to '{}'", ACTIVE_DB_FILENAME, last_slug, last_game_archive_path.display());
                    // Attempt to rename the active DB file to its archived name.
                    if let Err(e) = fs::rename(&active_db_path, &last_game_archive_path) {
                         // If renaming fails, it's a critical error preventing the switch.
                         let err_msg = format!("Failed to archive DB for '{}': {}", last_slug, e);
                         eprintln!("FATAL: {}", err_msg);
                         dialog::blocking::message(
                             app_handle.get_window("main").as_ref(),
                             "Fatal Startup Error",
                             &err_msg
                         );
                         std::process::exit(1);
                    }
                } else {
                     // Log a warning if the active file doesn't exist, as it might indicate a previous issue.
                     println!("Warning: {} not found, cannot archive game '{}'.", ACTIVE_DB_FILENAME, last_slug);
                }

                // Step B: Activate the requested DB by renaming its archive file (if it exists) to the active name.
                if requested_game_archive_path.exists() {
                     println!("Activating '{}' from '{}'", ACTIVE_DB_FILENAME, requested_game_archive_path.display());
                     // Attempt to rename the requested game's archive to the active DB name.
                     if let Err(e) = fs::rename(&requested_game_archive_path, &active_db_path) {
                          // If this rename fails, try to roll back the first rename (Step A) if possible.
                          if last_game_archive_path.exists() {
                              println!("Attempting rollback: Renaming {} back to {}", last_game_archive_path.display(), active_db_path.display());
                              fs::rename(&last_game_archive_path, &active_db_path).ok(); // Ignore rollback error, main error is critical.
                          }
                          // Report the critical error that prevented activation.
                          let err_msg = format!("Failed to activate DB for '{}': {}", requested_slug, e);
                          eprintln!("FATAL: {}", err_msg);
                          dialog::blocking::message(
                              app_handle.get_window("main").as_ref(),
                              "Fatal Startup Error",
                              &err_msg
                          );
                          std::process::exit(1);
                     }
                } else {
                     // If the requested game's archive doesn't exist, a new DB will be created later by initialize_database.
                     println!("Archive for requested game '{}' ('{}') not found. New DB will be created.", requested_slug, requested_game_archive_path.display());
                }

                // Step C: Update the configuration file to reflect the successful switch.
                // The 'last_active_game' should now match the 'requested_active_game'.
                println!("Updating config to set last_active_game = requested_active_game ('{}')", requested_slug);
                config.last_active_game = requested_slug.clone(); // Update the config struct in memory.
                if let Err(e) = write_app_config(&app_handle, &config) {
                     // If writing the config fails, the state is inconsistent. Log a critical warning.
                     // The app will likely function for this session, but the next startup might be incorrect.
                     eprintln!("CRITICAL WARNING: Failed to update config after DB rename: {}. Config may be out of sync!", e);
                } else {
                     println!("Config synced successfully.");
                }
                println!("DB swap/activation completed for '{}'.", requested_slug);

            } else {
                // If last_slug and requested_slug are the same, no switch is needed.
                println!("No game switch needed (Last Active == Requested Active: '{}').", requested_slug);
                // As a sanity check, ensure the active DB file actually exists.
                 let active_db_path = get_app_data_dir(&app_handle).expect("Data dir checked previously").join(ACTIVE_DB_FILENAME);
                 if !active_db_path.exists() {
                     println!("Warning: Config indicates no switch needed, but '{}' does not exist. A new DB will be created for '{}'.", ACTIVE_DB_FILENAME, requested_slug);
                 }
            }
            println!("Pre-initialization DB check complete.");

            // --- 3. Initialize DB Connection for State ---
            // Initialize the database connection using the (now correctly named) active DB file.
            // Pass the slug of the game that *should* be active now (the requested_slug).
            let conn = match initialize_database(&app_handle, requested_slug) {
                 Ok(c) => c,
                 Err(e) => {
                     // If database initialization fails (e.g., cannot open/create file, schema error).
                     eprintln!("FATAL: Database initialization failed: {}", e);
                     dialog::blocking::message(
                         app_handle.get_window("main").as_ref(),
                         "Fatal Database Error",
                         &format!("DB init failed for {}: {}", ACTIVE_DB_FILENAME, e)
                     );
                     std::process::exit(1);
                 }
            };
            println!("Database connection established for {}.", ACTIVE_DB_FILENAME);

            // --- 4. Manage State & Final Checks ---
            // Make the database connection available to Tauri commands via managed state.
             app.manage(DbState(Arc::new(Mutex::new(conn))));

             // --- *** ADD MIGRATION CHECK *** ---
            println!("--- Running Post-Init Checks/Migrations ---");
            let db_state_for_migration: State<DbState> = app.state(); // Get the managed state again
            let app_handle_for_migration = app.handle(); // Clone handle for migration logic
            match run_traveler_migration_logic(&db_state_for_migration, &app_handle_for_migration) {
                 Ok(msg) => println!("[Setup Migration Check] {}", msg), // Log success/skip message
                 Err(e) => {
                     // Log the error, but don't necessarily crash the app unless it's critical
                     eprintln!("[Setup Migration Check] WARNING: Traveler migration check/run failed: {}", e);
                     // Optionally show a non-fatal dialog to the user?
                     // dialog::blocking::message(
                     //    app_handle.get_window("main").as_ref(),
                     //    "Migration Warning",
                     //    &format!("An automatic data migration (Traveler -> Aether/Lumine) could not be completed:\n\n{}\n\nYou may need to run it manually via settings later.", e)
                     // );
                 }
            }
            println!("--- Finished Post-Init Checks/Migrations ---");
            // --- *** END MIGRATION CHECK *** ---

             // Perform a final check/log for a key setting (like mods folder) from the *active* DB.
             let db_state: State<DbState> = app.state(); // Get the managed state.
             match get_setting_value(&db_state.0.lock().expect("DB lock poisoned during setup check"), SETTINGS_KEY_MODS_FOLDER) { // Lock mutex to access connection.
                 Ok(Some(path)) => println!("Mods folder configured in active DB to: {}", path),
                 _ => println!("WARN: Mods folder path is not configured yet in active DB."),
             }
             println!("--- Application Setup Complete ---");
            Ok(()) // Indicate successful setup
        })
        .invoke_handler(generate_handler![
            // List ALL exposed Tauri commands here:
            // Settings
            get_setting, set_setting, select_directory, select_file, launch_executable,
            launch_executable_elevated,
            // Core
            get_categories, get_category_entities, get_entities_by_category,
            get_entity_details, get_assets_for_entity, toggle_asset_enabled,
            get_asset_image_path, run_traveler_migration,
            open_mods_folder,
            // Scan & Count
            scan_mods_directory, get_total_asset_count,
            get_entities_by_category_with_counts,
            // Edit, Import, Delete (Assets)
            update_asset_info, delete_asset, read_binary_file,
            select_archive_file, analyze_archive,
            import_archive,
            read_archive_file_content,
            // Presets
            create_preset, get_presets, get_favorite_presets, apply_preset,
            toggle_preset_favorite, delete_preset, overwrite_preset,
            add_asset_to_presets,
            // Dashboard & Version
            get_dashboard_stats, get_app_version,
            // Keybinds
            get_ini_keybinds, open_asset_folder,
            // Multi-Game Commands
            get_available_games, get_active_game, switch_game,
            exit_app
        ])
        .run(context) // Runs the Tauri application loop.
        .expect("error while running tauri application"); // Panic if the app fails to run.
}