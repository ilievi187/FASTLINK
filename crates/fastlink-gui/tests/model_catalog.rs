//! Session-local GGUF path registration.
//!
//! These tests use one explicit temporary file per case. They do not scan
//! directories, copy weights, or read file contents.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fastlink_gui::inventory::InventoryCommand;
use fastlink_gui::models::{register_gguf_path, ModelCatalog, ModelPathError};
use fastlink_gui::state::AppState;

fn scratch_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("fastlink-gguf-{label}-{nanos}"));
    fs::create_dir(&dir).expect("scratch dir");
    dir
}

fn write_named(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    let mut file = File::create(&path).expect("create file");
    file.write_all(bytes).expect("write marker");
    path
}

#[test]
fn readable_gguf_records_metadata_without_capturing_weight_bytes() {
    let dir = scratch_dir("meta");
    let marker = b"NOT-A-GGUF-WEIGHT-PAYLOAD";
    let path = write_named(&dir, "session-model.gguf", marker);

    let registered = register_gguf_path(&path).expect("readable .gguf registers");

    assert_eq!(registered.path, path);
    assert_eq!(registered.file_name, "session-model.gguf");
    assert_eq!(registered.size_bytes, marker.len() as u64);
    assert!(!registered.loaded);
    let rendered = format!("{registered:?}");
    assert!(
        !rendered.contains("NOT-A-GGUF-WEIGHT-PAYLOAD"),
        "registration must not retain weight bytes: {rendered}"
    );
    assert_eq!(fs::read(&path).expect("source unchanged"), marker);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn uppercase_gguf_extension_is_accepted_as_metadata() {
    let dir = scratch_dir("upper");
    let path = write_named(&dir, "Model.GGUF", b"abc");

    let registered = register_gguf_path(&path).expect("uppercase extension");

    assert_eq!(registered.file_name, "Model.GGUF");
    assert_eq!(registered.size_bytes, 3);
    assert!(!registered.loaded);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn non_gguf_extension_is_rejected_without_registering_it() {
    let dir = scratch_dir("ext");
    let path = write_named(&dir, "notes.txt", b"hello");

    let error = register_gguf_path(&path).expect_err("only .gguf paths register");

    assert_eq!(error, ModelPathError::NotGguf);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_gguf_path_is_rejected() {
    let dir = scratch_dir("missing");
    let path = dir.join("absent.gguf");

    let error = register_gguf_path(&path).expect_err("missing path");

    assert_eq!(error, ModelPathError::Missing);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn directory_named_gguf_is_rejected_and_children_are_not_registered() {
    let dir = scratch_dir("dir");
    let folder = dir.join("weights.gguf");
    fs::create_dir(&folder).expect("directory");
    write_named(&folder, "child.gguf", b"child-weight");

    let error = register_gguf_path(&folder).expect_err("directory is not a file");

    assert_eq!(error, ModelPathError::NotAFile);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unreadable_gguf_is_rejected() {
    let dir = scratch_dir("mode");
    let path = write_named(&dir, "secret.gguf", b"weights");
    let mut permissions = fs::metadata(&path).expect("meta").permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o000);
    }
    #[cfg(not(unix))]
    {
        permissions.set_readonly(true);
    }
    fs::set_permissions(&path, permissions).expect("chmod");

    let error = register_gguf_path(&path).expect_err("unreadable file");

    assert_eq!(error, ModelPathError::NotReadable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut restore = fs::metadata(&path).expect("meta").permissions();
        restore.set_mode(0o600);
        fs::set_permissions(&path, restore).expect("restore");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn catalog_registers_only_the_explicit_path_and_ignores_siblings() {
    let dir = scratch_dir("catalog");
    let chosen = write_named(&dir, "chosen.gguf", b"chosen");
    let _sibling = write_named(&dir, "sibling.gguf", b"sibling");
    let mut catalog = ModelCatalog::new();

    catalog.set_draft(chosen.to_str().expect("utf8 path"));
    assert!(catalog.register_draft());

    assert_eq!(catalog.entries().len(), 1);
    assert_eq!(catalog.entries()[0].path, chosen);
    assert_eq!(catalog.entries()[0].size_bytes, 6);
    assert!(!catalog.entries()[0].loaded);
    assert!(catalog.error().is_none());
    assert!(catalog.load_confirmation().is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn registering_the_same_path_again_keeps_a_single_entry() {
    let dir = scratch_dir("dup");
    let path = write_named(&dir, "once.gguf", b"abcdef");
    let mut catalog = ModelCatalog::new();
    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());

    assert_eq!(catalog.entries().len(), 1);
    assert_eq!(catalog.selected_index(), Some(0));
    assert!(catalog.error().is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn confirm_load_marks_pending_backend_without_loading_weights() {
    let dir = scratch_dir("confirm");
    let path = write_named(&dir, "pending.gguf", b"weights-stay-on-disk");
    let mut catalog = ModelCatalog::new();
    assert!(!catalog.confirm_load());
    assert!(catalog.load_confirmation().is_none());

    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert!(catalog.confirm_load());

    assert_eq!(catalog.load_confirmation(), Some(path.as_path()));
    assert!(!catalog.entries()[0].loaded);
    assert!(!catalog.weights_loaded());
    let before = fs::read(&path).expect("read source");
    assert_eq!(before, b"weights-stay-on-disk");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn selecting_a_different_model_clears_the_load_confirmation() {
    let dir = scratch_dir("select");
    let first = write_named(&dir, "first.gguf", b"one");
    let second = write_named(&dir, "second.gguf", b"two");
    let mut catalog = ModelCatalog::new();
    catalog.set_draft(first.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert!(catalog.confirm_load());
    catalog.set_draft(second.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert_eq!(catalog.selected_index(), Some(1));
    assert!(catalog.load_confirmation().is_none());
    assert!(catalog.select(0));
    assert!(catalog.load_confirmation().is_none());
    assert!(!catalog.entries().iter().any(|entry| entry.loaded));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn blank_draft_is_rejected_before_any_filesystem_lookup() {
    let mut catalog = ModelCatalog::new();
    catalog.set_draft("   \t");
    assert!(!catalog.register_draft());
    assert_eq!(catalog.error(), Some(&ModelPathError::Empty));
    assert!(catalog.entries().is_empty());
    assert!(catalog.load_confirmation().is_none());
}

#[test]
fn remove_selected_drops_the_entry_and_its_load_confirmation() {
    let dir = scratch_dir("remove");
    let path = write_named(&dir, "gone.gguf", b"gone");
    let mut catalog = ModelCatalog::new();
    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert!(catalog.confirm_load());
    assert!(catalog.remove_selected());

    assert!(catalog.entries().is_empty());
    assert!(catalog.selected_index().is_none());
    assert!(catalog.load_confirmation().is_none());
    assert!(!catalog.weights_loaded());
    assert_eq!(fs::read(&path).expect("file stays on disk"), b"gone");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn confirmed_registration_keeps_chat_unavailable_and_discovery_idle() {
    let dir = scratch_dir("session");
    let path = write_named(&dir, "local.gguf", b"metadata-only");
    let mut state = AppState::new(InventoryCommand::new("fastlink"));

    assert!(state.discovery().is_none());
    assert!(!state.discovery_busy());
    assert!(!state.chat_available());
    assert!(!state.inference_backend_connected());
    assert!(state.generated_tokens().is_empty());
    assert!(state.model_catalog().entries().is_empty());

    state
        .model_catalog_mut()
        .set_draft(path.to_str().expect("utf8"));
    assert!(state.model_catalog_mut().register_draft());
    assert!(state.model_catalog_mut().confirm_load());

    assert!(!state.chat_available());
    assert!(!state.inference_backend_connected());
    assert!(state.generated_tokens().is_empty());
    assert!(!state.model_catalog().weights_loaded());
    assert_eq!(
        state.model_catalog().load_confirmation(),
        Some(path.as_path())
    );
    assert!(state.discovery().is_none());
    assert!(!state.discovery_busy());
    assert_eq!(state.screen(), fastlink_gui::state::Screen::Idle);
    state.apply_result(Err(fastlink_gui::inventory::LoadError::new(
        "inventory failed",
    )));
    assert_eq!(state.model_catalog().entries().len(), 1);
    assert_eq!(
        state.model_catalog().load_confirmation(),
        Some(path.as_path())
    );

    let fresh = AppState::new(InventoryCommand::new("fastlink"));
    assert!(fresh.model_catalog().entries().is_empty());
    assert!(fresh.model_catalog().load_confirmation().is_none());

    let _ = fs::remove_dir_all(&dir);
}

fn painted_text(state: &mut AppState) -> String {
    use egui::{Context, Pos2, RawInput, Rect, Vec2};
    let ctx = Context::default();
    let size = Vec2::new(1280.0, 1600.0);
    let mut collected = String::new();
    for _ in 0..2 {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| fastlink_gui::ui::draw(ui, state));
        for clipped in &output.shapes {
            collect_shape(&clipped.shape, &mut collected);
        }
        let primitives = ctx.tessellate(output.shapes.clone(), output.pixels_per_point);
        output.textures_delta.clear();
        assert!(!primitives.is_empty(), "catalog UI must paint");
    }
    collected
}

fn collect_shape(shape: &egui::Shape, out: &mut String) {
    match shape {
        egui::Shape::Text(text) => {
            out.push_str(text.galley.text());
            out.push('\n');
        }
        egui::Shape::Vec(children) => {
            for child in children {
                collect_shape(child, out);
            }
        }
        _ => {}
    }
}

#[test]
fn catalog_ui_shows_metadata_confirmation_and_unavailable_chat() {
    let dir = scratch_dir("ui");
    let marker = b"WEIGHT-BYTES-MUST-NOT-RENDER";
    let path = write_named(&dir, "visible.gguf", marker);
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state
        .model_catalog_mut()
        .set_draft(path.to_str().expect("utf8"));
    assert!(state.model_catalog_mut().register_draft());
    assert!(state.model_catalog_mut().confirm_load());

    let text = painted_text(&mut state);
    assert!(
        text.contains("Register GGUF path"),
        "missing register action:\n{text}"
    );
    assert!(text.contains("visible.gguf"), "missing file name:\n{text}");
    assert!(
        text.contains("Not loaded"),
        "missing not-loaded state:\n{text}"
    );
    assert!(
        text.contains("Load confirmed"),
        "missing confirmation:\n{text}"
    );
    assert!(
        text.contains("No inference backend is connected"),
        "missing backend notice:\n{text}"
    );
    assert!(
        text.contains("does not generate tokens"),
        "missing token notice:\n{text}"
    );
    assert!(
        text.contains("Not available"),
        "chat must stay unavailable:\n{text}"
    );
    assert!(
        text.contains("Discover devices"),
        "discovery action must remain:\n{text}"
    );
    assert!(
        !text.contains("WEIGHT-BYTES-MUST-NOT-RENDER"),
        "weight bytes rendered:\n{text}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn validation_error_is_visible_and_does_not_clear_an_existing_entry() {
    let dir = scratch_dir("errui");
    let kept = write_named(&dir, "kept.gguf", b"kept");
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state
        .model_catalog_mut()
        .set_draft(kept.to_str().expect("utf8"));
    assert!(state.model_catalog_mut().register_draft());
    assert!(state.model_catalog_mut().confirm_load());

    state
        .model_catalog_mut()
        .set_draft(dir.join("notes.txt").to_str().expect("utf8"));
    assert!(!state.model_catalog_mut().register_draft());
    assert_eq!(
        state
            .model_catalog()
            .error()
            .copied()
            .map(ModelPathError::summary),
        Some("The path must end in .gguf. Other files are not registered.")
    );
    assert_eq!(state.model_catalog().entries().len(), 1);
    assert_eq!(
        state.model_catalog().load_confirmation(),
        Some(kept.as_path())
    );

    let text = painted_text(&mut state);
    assert!(
        text.contains("The path must end in .gguf. Other files are not registered."),
        "validation error not painted:\n{text}"
    );
    assert!(
        text.contains("kept.gguf"),
        "existing entry disappeared:\n{text}"
    );
    assert!(
        text.contains("Load confirmed"),
        "confirmation disappeared:\n{text}"
    );
    assert!(text.contains("Not available"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn same_file_through_dotdot_collapses_to_one_catalog_entry() {
    let dir = scratch_dir("canon");
    let nested = dir.join("nested");
    fs::create_dir(&nested).expect("nested");
    let path = write_named(&dir, "same.gguf", b"same");
    let alias = nested.join("../same.gguf");
    let mut catalog = ModelCatalog::new();
    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    catalog.set_draft(alias.to_str().expect("utf8"));
    assert!(catalog.register_draft());

    assert_eq!(catalog.entries().len(), 1);
    assert_eq!(catalog.entries()[0].path, path);
    assert_eq!(catalog.selected_index(), Some(0));

    let _ = fs::remove_dir_all(&dir);
}
