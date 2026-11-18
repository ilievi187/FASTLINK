//! Session-local GGUF path registration.
//!
//! These tests use one explicit temporary file per case. They do not scan
//! directories, copy weights, or read file contents.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fastlink_gui::inventory::InventoryCommand;
use fastlink_gui::models::{
    chat_status_text, loaded_status_label, map_metadata_error, redacted_path,
    refused_windows_file_name, refused_windows_namespace, register_gguf_path,
    sanitize_rendered_text, ModelCatalog, ModelPathError,
};
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
fn windows_namespace_forms_are_classified_without_touching_the_filesystem() {
    for raw in [
        r"\\.\pipe\fastlink.gguf",
        r"\\.\C:\models\x.gguf",
        r"\\?\pipe\fastlink.gguf",
        r"\\?\UNC\server\share\x.gguf",
        r"\\?\GLOBALROOT\Device\HarddiskVolume1\x.gguf",
        r"\??\pipe\fastlink.gguf",
        r"\\server\share\x.gguf",
        r"\\server\pipe\fastlink.gguf",
        "//server/pipe/fastlink.gguf",
    ] {
        assert!(refused_windows_namespace(Path::new(raw)), "{raw}");
    }

    for raw in [
        r"C:\models\model.gguf",
        r"c:models\model.gguf",
        r"\\?\C:\models\model.gguf",
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\models\model.gguf",
        "/tmp/model.gguf",
        "models/model.gguf",
    ] {
        assert!(!refused_windows_namespace(Path::new(raw)), "{raw}");
    }
}

#[test]
fn windows_reserved_names_are_flagged_independently_of_the_host() {
    for raw in [
        "NUL.gguf",
        "nul",
        "CON.gguf",
        "con .gguf",
        "PRN.gguf",
        "AUX.gguf",
        "COM1.gguf",
        "lpt9.gguf",
        "COM\u{00b9}.gguf",
    ] {
        assert!(refused_windows_file_name(Path::new(raw)), "{raw}");
    }
    for raw in ["model.gguf", "auxiliary.gguf", "com10.gguf", "console.gguf"] {
        assert!(!refused_windows_file_name(Path::new(raw)), "{raw}");
    }
}

#[cfg(not(windows))]
#[test]
fn dos_looking_names_remain_valid_linux_files() {
    let dir = scratch_dir("doslinux");
    for name in [
        "NUL.gguf",
        "COM1.gguf",
        "LPT9.gguf",
        "AUX.gguf",
        "con .gguf",
    ] {
        let path = write_named(&dir, name, b"weights");
        let entry = register_gguf_path(&path).expect("a Linux file with a DOS-looking name");
        assert_eq!(entry.size_bytes, 7, "{name}");
        assert_eq!(entry.path, path, "{name}");
        assert!(!entry.loaded, "{name}");
    }

    let mut catalog = ModelCatalog::new();
    let nul = dir.join("NUL.gguf");
    catalog.set_draft(nul.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert_eq!(catalog.entries().len(), 1);
    assert_eq!(catalog.entries()[0].path, nul);

    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state
        .model_catalog_mut()
        .set_draft(nul.to_str().expect("utf8"));
    assert!(state.request_register_model());
    assert!(wait_until(&mut state, |state| {
        state.model_catalog().entries().len() == 1 && !state.model_fs_busy()
    }));
    assert!(state.model_catalog().error().is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn windows_namespace_and_reserved_names_are_refused_before_open() {
    for raw in [
        r"\\.\pipe\fastlink.gguf",
        r"\\?\pipe\fastlink.gguf",
        r"\??\pipe\fastlink.gguf",
        r"\\localhost\pipe\fastlink.gguf",
        r"\\server\share\fastlink.gguf",
        "NUL.gguf",
        "COM1.gguf",
        "LPT9.gguf",
    ] {
        assert_eq!(
            register_gguf_path(Path::new(raw)),
            Err(ModelPathError::NotAFile),
            "{raw}"
        );
    }

    // A valid extended-length local path must still register.
    let dir = scratch_dir("extended");
    let file = write_named(&dir, "extended.gguf", b"abcd");
    let canonical = fs::canonicalize(&file).expect("canonical extended path");
    assert!(
        canonical.to_string_lossy().starts_with(r"\\?\"),
        "expected an extended-length canonical path: {}",
        canonical.display()
    );
    let entry = register_gguf_path(&canonical).expect("extended local path stays valid");
    assert_eq!(entry.size_bytes, 4);
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
fn metadata_errors_keep_not_found_permission_and_other_distinct() {
    use std::io::{Error, ErrorKind};

    assert_eq!(
        map_metadata_error(&Error::new(ErrorKind::NotFound, "gone")),
        ModelPathError::Missing
    );
    assert_eq!(
        map_metadata_error(&Error::new(ErrorKind::PermissionDenied, "denied")),
        ModelPathError::NotReadable
    );
    let other = Error::other("io");
    assert_eq!(map_metadata_error(&other), ModelPathError::Unavailable);
    assert!(!ModelPathError::Unavailable
        .summary()
        .contains("does not exist"));
    assert!(ModelPathError::NotReadable
        .summary()
        .contains("not readable"));
}

#[test]
fn unreadable_result_follows_actual_open_not_a_readonly_bit() {
    let dir = scratch_dir("mode");
    let path = write_named(&dir, "secret.gguf", b"weights");
    let original = fs::metadata(&path).expect("meta").permissions();
    let mut denied = original.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        denied.set_mode(0o000);
    }
    #[cfg(not(unix))]
    {
        denied.set_readonly(true);
    }
    fs::set_permissions(&path, denied).expect("restrict");

    let opened = File::open(&path);
    let registered = register_gguf_path(&path);
    match opened {
        Ok(_) => {
            let entry = registered.expect("an openable file stays registrable");
            assert_eq!(entry.size_bytes, b"weights".len() as u64);
            assert!(!entry.loaded);
        }
        Err(err) => {
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(
                registered.expect_err("open failure"),
                ModelPathError::NotReadable
            );
        }
    }

    let _ = fs::set_permissions(&path, original);
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
fn successful_registration_clears_the_draft_and_a_failure_keeps_it() {
    let dir = scratch_dir("draft");
    let path = write_named(&dir, "kept.gguf", b"abcd");
    let mut catalog = ModelCatalog::new();
    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert_eq!(catalog.draft(), "");

    let rejected = dir.join("notes.txt");
    catalog.set_draft(rejected.to_str().expect("utf8"));
    assert!(!catalog.register_draft());
    assert_eq!(catalog.draft(), rejected.to_str().expect("utf8"));
    assert_eq!(catalog.entries().len(), 1);

    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state
        .model_catalog_mut()
        .set_draft(path.to_str().expect("utf8"));
    assert!(state.request_register_model());
    state
        .model_catalog_mut()
        .set_draft(rejected.to_str().expect("utf8"));
    assert!(wait_until(&mut state, |state| !state.model_fs_busy()));
    assert_eq!(state.model_catalog().entries().len(), 1);
    assert_eq!(
        state.model_catalog().draft(),
        rejected.to_str().expect("utf8"),
        "a draft typed while the check runs must stay"
    );

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
fn catalog_ui_shows_metadata_confirmation_and_chat_handoff() {
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
    assert!(text.contains("Confirmed"), "missing confirmation:\n{text}");
    assert!(
        text.contains("Open Chat to load it."),
        "missing Chat handoff:\n{text}"
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
        text.contains("Confirmed"),
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
    assert!(!catalog.select(4));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rendered_name_and_path_replace_control_and_bidi_characters() {
    use fastlink_gui::models::sanitize_rendered_text;

    let spoofed = "safe\u{202E}\u{0007}name.gguf";
    let cleaned = sanitize_rendered_text(spoofed);
    assert!(!cleaned.contains('\u{202E}'));
    assert!(!cleaned.contains('\u{0007}'));
    assert!(cleaned.contains('\u{FFFD}'));
    assert!(!cleaned.chars().any(|c| c.is_control()));

    let dir = scratch_dir("bidi");
    let path = write_named(&dir, "ok\u{202E}name.gguf", b"abcd");
    let registered = register_gguf_path(&path).expect("bidi name");
    assert!(!registered.file_name.contains('\u{202E}'));
    assert!(registered.file_name.contains('\u{FFFD}'));
    let shown = redacted_path(&path);
    assert!(!shown.contains('\u{202E}'));
    assert!(!shown.chars().any(|c| c.is_control()));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sanitizer_replaces_line_separators_tags_and_invisible_fillers() {
    let raw = "a\u{2028}b\u{2029}c\u{FFF9}d\u{E0001}e\u{E0041}f\u{034F}g\u{115F}h\u{3164}i\u{FFA0}j\u{2800}k";
    let cleaned = sanitize_rendered_text(raw);
    for bad in [
        '\u{2028}',
        '\u{2029}',
        '\u{FFF9}',
        '\u{E0001}',
        '\u{E0041}',
        '\u{034F}',
        '\u{115F}',
        '\u{3164}',
        '\u{FFA0}',
        '\u{2800}',
    ] {
        assert!(!cleaned.contains(bad), "{bad:?} survived: {cleaned:?}");
    }
    assert_eq!(cleaned.chars().filter(|c| *c == '\u{FFFD}').count(), 10);
    assert!(cleaned.contains('a') && cleaned.contains('k'));

    let dir = scratch_dir("linechars");
    let path = write_named(&dir, "line\u{2028}name.gguf", b"abcd");
    let registered = register_gguf_path(&path).expect("line separator in the name");
    assert!(!registered.file_name.contains('\u{2028}'));
    assert!(registered.file_name.contains('\u{FFFD}'));
    let shown = redacted_path(&path);
    assert!(!shown.contains('\u{2028}'));
    assert!(!shown.chars().any(char::is_control));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn redaction_strips_either_home_separator_and_truncates() {
    use fastlink_gui::models::redact_path_text;

    let shown = redact_path_text(r"C:\Users\secret\models\unit.gguf", &[r"C:\Users\secret"]);
    assert!(!shown.to_ascii_lowercase().contains("secret"), "{shown}");
    assert!(shown.starts_with('~'), "{shown}");
    assert!(shown.contains("unit.gguf"), "{shown}");

    let long = format!("D:/Users/secret/{}", "a".repeat(80));
    let shown = redact_path_text(&long, &["d:/users/secret"]);
    assert!(!shown.to_ascii_lowercase().contains("secret"), "{shown}");
    assert!(
        shown.chars().count() <= 48,
        "{shown} len {}",
        shown.chars().count()
    );
    assert!(shown.starts_with('…'), "{shown}");
}

#[test]
fn redacted_path_drops_the_absolute_prefix_and_home_directory() {
    let marker = "SECRETMARKER";
    let path = PathBuf::from(format!("/{marker}/{:a<80}/visible.gguf", ""));
    let shown = redacted_path(&path);
    assert!(!shown.contains(marker), "{shown}");
    assert!(shown.starts_with('…'), "{shown}");
    assert!(shown.contains("visible.gguf"), "{shown}");

    if let Some(home) = std::env::var_os("HOME") {
        let home_text = home.to_string_lossy().into_owned();
        if !home_text.is_empty() {
            let under = PathBuf::from(&home).join("models").join("unit.gguf");
            let shown_home = redacted_path(&under);
            assert!(!shown_home.contains(&home_text), "{shown_home}");
            assert!(
                shown_home.starts_with('~') || shown_home.starts_with('…'),
                "{shown_home}"
            );
        }
    }
}

#[test]
fn duplicate_registration_refreshes_size_and_confirm_rejects_a_deleted_file() {
    let dir = scratch_dir("stale");
    let path = write_named(&dir, "live.gguf", b"old");
    let mut catalog = ModelCatalog::new();
    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert_eq!(catalog.entries()[0].size_bytes, 3);

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append");
    file.write_all(b"MORE").expect("grow");
    drop(file);
    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert_eq!(catalog.entries().len(), 1);
    assert_eq!(catalog.entries()[0].size_bytes, 7);
    assert_eq!(catalog.entries()[0].path, path);

    fs::remove_file(&path).expect("delete");
    assert!(!catalog.confirm_load());
    assert_eq!(catalog.error(), Some(&ModelPathError::Missing));
    assert!(catalog.load_confirmation().is_none());
    assert!(!catalog.entries()[0].loaded);
    assert_eq!(loaded_status_label(catalog.weights_loaded()), "Not loaded");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn confirmed_path_can_be_rechecked_and_failed_reregistration_clears_it() {
    let dir = scratch_dir("recheck");
    let path = write_named(&dir, "live.gguf", b"old");
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state
        .model_catalog_mut()
        .set_draft(path.to_str().expect("utf8"));
    assert!(state.request_register_model());
    assert!(wait_until(&mut state, |state| {
        state.model_catalog().entries().len() == 1 && !state.model_fs_busy()
    }));
    assert!(state.request_confirm_model_load());
    assert!(wait_until(&mut state, |state| {
        !state.model_fs_busy() && state.model_catalog().load_confirmation().is_some()
    }));

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("append");
    file.write_all(b"MORE").expect("grow");
    drop(file);
    assert!(
        state.request_confirm_model_load(),
        "an already confirmed path must be re-checked"
    );
    assert!(wait_until(&mut state, |state| {
        !state.model_fs_busy()
            && state
                .model_catalog()
                .entries()
                .first()
                .map(|entry| entry.size_bytes)
                == Some(7)
    }));
    assert_eq!(
        state.model_catalog().load_confirmation(),
        Some(path.as_path())
    );
    assert!(!state.model_catalog().entries()[0].loaded);

    fs::remove_file(&path).expect("delete");
    state
        .model_catalog_mut()
        .set_draft(path.to_str().expect("utf8"));
    assert!(state.request_register_model());
    assert!(wait_until(&mut state, |state| !state.model_fs_busy()));
    assert_eq!(
        state.model_catalog().error(),
        Some(&ModelPathError::Missing)
    );
    assert!(state.model_catalog().load_confirmation().is_none());
    assert_eq!(state.model_catalog().entries().len(), 1);

    let _ = fs::remove_dir_all(&dir);
}

fn wait_until(state: &mut AppState, mut ready: impl FnMut(&AppState) -> bool) -> bool {
    use std::thread;
    use std::time::Duration;
    for _ in 0..50 {
        state.poll();
        if ready(state) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn chat_and_loaded_labels_follow_explicit_flags() {
    assert_eq!(loaded_status_label(false), "Not loaded");
    assert_eq!(loaded_status_label(true), "Loaded");
    let (title, body) = chat_status_text(false, true);
    assert_eq!(title, "Not available");
    assert!(body.contains("No inference backend is connected"));
    assert!(body.contains("does not generate tokens"));
    let (title, _) = chat_status_text(true, true);
    assert_eq!(title, "Available");
}

#[test]
fn in_flight_model_job_cannot_be_retargeted_by_selection_or_removal() {
    use std::sync::mpsc;

    use fastlink_gui::models::{ModelJob, ModelJobResult};

    let dir = scratch_dir("inflight");
    let first = write_named(&dir, "first.gguf", b"aaaa");
    let second = write_named(&dir, "second.gguf", b"bbbb");
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state
        .model_catalog_mut()
        .set_draft(first.to_str().expect("utf8"));
    assert!(state.model_catalog_mut().register_draft());
    state
        .model_catalog_mut()
        .set_draft(second.to_str().expect("utf8"));
    assert!(state.model_catalog_mut().register_draft());
    assert_eq!(state.model_catalog().entries().len(), 2);
    assert!(state.model_catalog_mut().select(0));

    let generation = state.model_catalog().generation();
    let (release, gate) = mpsc::channel();
    let stale_path = first.clone();
    assert!(state
        .model_loader_mut()
        .start_with(move || {
            let _ = gate.recv();
            Ok(ModelJobResult {
                job: ModelJob::Register,
                path: stale_path.clone(),
                generation,
                outcome: Ok(fastlink_gui::models::RegisteredGguf {
                    path: stale_path,
                    identity: PathBuf::from("/tmp/fastlink-stale-identity.gguf"),
                    opened_identity: fastlink_gui::models::OpenedFileIdentity::Path(PathBuf::from(
                        "/tmp/fastlink-stale-identity.gguf",
                    )),
                    file_name: "hijack.gguf".to_owned(),
                    size_bytes: 1,
                    loaded: false,
                }),
            })
        })
        .expect("hold model worker"));
    assert!(state.model_fs_busy());
    assert!(!state.select_registered_model(1));
    assert_eq!(state.model_catalog().selected_index(), Some(0));
    assert!(!state.remove_selected_model());
    assert_eq!(state.model_catalog().entries().len(), 2);

    assert!(state.model_catalog_mut().remove_selected());
    assert_eq!(state.model_catalog().entries().len(), 1);
    assert_eq!(state.model_catalog().entries()[0].path, second);
    release.send(()).expect("release");
    assert!(wait_until(&mut state, |state| !state.model_fs_busy()));
    assert_eq!(state.model_catalog().entries().len(), 1);
    assert_eq!(state.model_catalog().entries()[0].path, second);
    assert_eq!(state.model_catalog().entries()[0].file_name, "second.gguf");
    assert!(
        state.model_catalog().error().is_none(),
        "a stale success must not surface an error"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn background_model_check_does_not_stall_discovery_poll_or_cancel() {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use fastlink_gui::discovery::parse_discovery_json;
    use fastlink_gui::models::{ModelJob, ModelJobResult};

    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    let (release, gate) = mpsc::channel();
    assert!(state
        .model_loader_mut()
        .start_with(move || {
            let _ = gate.recv();
            Ok(ModelJobResult {
                job: ModelJob::Register,
                generation: 0,
                path: PathBuf::new(),
                outcome: Err(ModelPathError::Unavailable),
            })
        })
        .expect("model worker starts"));
    assert!(state.model_fs_busy());
    assert!(!state.request_register_model());
    assert!(!state.request_confirm_model_load());

    let fixture = include_str!("../testdata/discovery-v1.json");
    assert!(state
        .discovery_loader_mut()
        .start_with(move || Ok(parse_discovery_json(fixture).expect("fixture")))
        .expect("discovery worker starts"));
    let mut saw = false;
    for _ in 0..50 {
        state.poll();
        if state.discovery().is_some() {
            saw = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        saw,
        "discovery result must apply while the model worker is blocked"
    );
    assert!(state.discovery_error().is_none());
    assert!(state.model_fs_busy());

    let (done, hold) = mpsc::channel();
    let fixture = include_str!("../testdata/discovery-v1.json");
    assert!(state
        .discovery_loader_mut()
        .start_with(move || {
            let _ = hold.recv();
            Ok(parse_discovery_json(fixture).expect("fixture"))
        })
        .expect("second discovery"));
    assert!(state.bind_discovery_cancel());
    assert!(state.cancel_discovery());
    assert!(state.discovery_cancelling());
    assert!(state.model_fs_busy());
    done.send(()).expect("release discovery");
    release.send(()).expect("release model");
    for _ in 0..50 {
        state.poll();
        if !state.model_fs_busy() && !state.discovery_busy() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!state.model_fs_busy());
    assert!(!state.discovery_busy());
}

#[test]
fn worker_registration_keeps_metadata_across_a_successful_inventory_refresh() {
    use std::thread;
    use std::time::Duration;

    use fastlink_gui::inventory::parse_inventory_json;

    let dir = scratch_dir("refresh");
    let path = write_named(&dir, "kept.gguf", b"abcd");
    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state
        .model_catalog_mut()
        .set_draft(path.to_str().expect("utf8"));
    assert!(state.request_register_model());
    assert!(state.discovery().is_none());
    assert!(!state.discovery_busy());
    let mut ready = false;
    for _ in 0..50 {
        state.poll();
        if state.model_catalog().entries().len() == 1 && !state.model_fs_busy() {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready);
    assert_eq!(state.model_catalog().entries()[0].size_bytes, 4);
    assert!(!state.model_catalog().entries()[0].loaded);

    let inventory =
        parse_inventory_json(include_str!("../testdata/devices-v1.json")).expect("device fixture");
    state.apply_result(Ok(inventory));
    assert_eq!(state.model_catalog().entries().len(), 1);
    assert!(state.model_catalog().load_confirmation().is_none());
    assert!(!state.chat_available());

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn fifo_and_directory_symlink_are_not_regular_files() {
    use std::os::unix::fs::symlink;
    use std::time::Instant;

    let dir = scratch_dir("fifo");
    let fifo = dir.join("queue.gguf");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("path");
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);
    let started = Instant::now();
    assert_eq!(
        register_gguf_path(&fifo).expect_err("fifo"),
        ModelPathError::NotAFile
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    let folder = dir.join("subdir");
    fs::create_dir(&folder).expect("dir");
    let link = dir.join("link.gguf");
    symlink(&folder, &link).expect("symlink");
    assert_eq!(
        register_gguf_path(&link).expect_err("symlink dir"),
        ModelPathError::NotAFile
    );

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn symlink_alias_keeps_the_canonical_file_name_and_identity() {
    use std::os::unix::fs::symlink;

    let dir = scratch_dir("alias");
    let real = write_named(&dir, "real-name.gguf", b"abcd");
    let link = dir.join("alias-name.gguf");
    symlink(&real, &link).expect("symlink");
    let canonical = fs::canonicalize(&real).expect("canonical");

    let via_link = register_gguf_path(&link).expect("symlink registers");
    assert_eq!(via_link.path, link);
    assert_eq!(via_link.identity, canonical);
    assert_eq!(via_link.file_name, "real-name.gguf");
    assert_eq!(via_link.size_bytes, 4);
    assert!(!via_link.loaded);

    let mut catalog = ModelCatalog::new();
    catalog.set_draft(real.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    catalog.set_draft(link.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert_eq!(catalog.entries().len(), 1);
    assert_eq!(catalog.entries()[0].path, real);
    assert_eq!(catalog.entries()[0].identity, canonical);
    assert_eq!(catalog.entries()[0].file_name, "real-name.gguf");
    assert_eq!(catalog.entries()[0].size_bytes, 4);

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn confirmation_rejects_an_identity_or_path_mismatch() {
    use std::os::unix::fs::symlink;

    let dir = scratch_dir("mismatch");
    let real = write_named(&dir, "real-name.gguf", b"abcd");
    let other = write_named(&dir, "other-name.gguf", b"zzzz");
    let link = dir.join("alias-name.gguf");
    symlink(&real, &link).expect("symlink");

    let mut catalog = ModelCatalog::new();
    catalog.set_draft(real.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert_eq!(catalog.entries()[0].file_name, "real-name.gguf");

    let mut swapped = catalog.entries()[0].clone();
    swapped.identity = fs::canonicalize(&other).expect("other identity");
    swapped.file_name = "other-name.gguf".to_owned();
    swapped.size_bytes = 99;
    assert!(!catalog.accept_confirmation(swapped));
    assert!(catalog.load_confirmation().is_none());
    assert_eq!(catalog.error(), Some(&ModelPathError::Changed));
    assert!(ModelPathError::Changed.summary().contains("changed"));
    assert_eq!(catalog.entries()[0].file_name, "real-name.gguf");
    assert_eq!(catalog.entries()[0].size_bytes, 4);
    assert_eq!(
        catalog.entries()[0].identity,
        fs::canonicalize(&real).expect("real")
    );

    assert!(catalog.confirm_load());
    assert!(catalog.load_confirmation().is_some());
    assert!(
        catalog.error().is_none(),
        "a clean confirmation clears the error"
    );
    let mut swapped_again = catalog.entries()[0].clone();
    swapped_again.identity = fs::canonicalize(&other).expect("other identity");
    assert!(!catalog.accept_confirmation(swapped_again));
    assert!(
        catalog.load_confirmation().is_none(),
        "a mismatched re-check must clear the stale confirmation"
    );
    assert_eq!(catalog.error(), Some(&ModelPathError::Changed));

    let alias = register_gguf_path(&link).expect("alias inspection");
    assert_ne!(alias.path, catalog.entries()[0].path);
    assert!(!catalog.accept_confirmation(alias));
    assert!(catalog.load_confirmation().is_none());
    assert_eq!(catalog.entries()[0].file_name, "real-name.gguf");
    assert_eq!(catalog.entries()[0].path, real);

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn replacing_the_file_at_the_same_path_invalidates_confirmation() {
    use std::os::unix::fs::MetadataExt;

    let dir = scratch_dir("samepath-replacement");
    let path = write_named(&dir, "model.gguf", b"original model");
    let replacement = write_named(&dir, "replacement.gguf", b"replacement model is larger");
    let original_meta = fs::metadata(&path).expect("original metadata");
    let replacement_meta = fs::metadata(&replacement).expect("replacement metadata");
    assert_ne!(
        (original_meta.dev(), original_meta.ino()),
        (replacement_meta.dev(), replacement_meta.ino()),
        "the test must replace the opened file with a distinct filesystem object"
    );

    let mut catalog = ModelCatalog::new();
    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert!(catalog.confirm_load());
    assert!(catalog.load_confirmation().is_some());

    fs::rename(&replacement, &path).expect("atomically replace the same pathname");
    let replaced_meta = fs::metadata(&path).expect("replacement at original path");
    assert_ne!(
        (original_meta.dev(), original_meta.ino()),
        (replaced_meta.dev(), replaced_meta.ino())
    );

    assert!(
        !catalog.confirm_load(),
        "rechecking a replacement file at the same pathname must fail closed"
    );
    assert_eq!(catalog.error(), Some(&ModelPathError::Changed));
    assert!(catalog.load_confirmation().is_none());
    assert_eq!(
        catalog.entries()[0].size_bytes,
        b"original model".len() as u64
    );

    catalog.set_draft(path.to_str().expect("utf8"));
    assert!(
        catalog.register_draft(),
        "explicit re-registration refreshes the row"
    );
    assert_eq!(catalog.entries().len(), 1);
    assert_eq!(
        catalog.entries()[0].size_bytes,
        b"replacement model is larger".len() as u64
    );
    assert!(catalog.load_confirmation().is_none());
    assert!(catalog.error().is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn repointed_alias_replaces_the_stale_row_instead_of_duplicating() {
    use std::os::unix::fs::symlink;

    let dir = scratch_dir("repoint");
    let first = write_named(&dir, "first-target.gguf", b"aaaa");
    let second = write_named(&dir, "second-target.gguf", b"bbbbbb");
    let link = dir.join("alias.gguf");
    symlink(&first, &link).expect("symlink");

    let mut catalog = ModelCatalog::new();
    catalog.set_draft(link.to_str().expect("utf8"));
    assert!(catalog.register_draft());
    assert!(catalog.confirm_load());
    assert!(catalog.load_confirmation().is_some());
    assert_eq!(catalog.entries()[0].file_name, "first-target.gguf");

    fs::remove_file(&link).expect("remove link");
    symlink(&second, &link).expect("repoint");
    catalog.set_draft(link.to_str().expect("utf8"));
    assert!(catalog.register_draft());

    assert_eq!(
        catalog.entries().len(),
        1,
        "a repointed alias must refresh the row, not add a duplicate"
    );
    let entry = &catalog.entries()[0];
    assert_eq!(entry.path, link);
    assert_eq!(entry.identity, fs::canonicalize(&second).expect("second"));
    assert_eq!(entry.file_name, "second-target.gguf");
    assert_eq!(entry.size_bytes, 6);
    assert!(
        catalog.load_confirmation().is_none(),
        "the stale confirmation for the old target must be cleared"
    );
    assert!(!entry.loaded);

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn state_confirm_surfaces_a_changed_identity_and_keeps_the_row() {
    use std::os::unix::fs::symlink;

    let dir = scratch_dir("statechanged");
    let first = write_named(&dir, "first-target.gguf", b"aaaa");
    let second = write_named(&dir, "second-target.gguf", b"bbbb");
    let link = dir.join("alias.gguf");
    symlink(&first, &link).expect("symlink");

    let mut state = AppState::new(InventoryCommand::new("fastlink"));
    state
        .model_catalog_mut()
        .set_draft(link.to_str().expect("utf8"));
    assert!(state.request_register_model());
    assert!(wait_until(&mut state, |state| {
        state.model_catalog().entries().len() == 1 && !state.model_fs_busy()
    }));
    assert!(state.request_confirm_model_load());
    assert!(wait_until(&mut state, |state| {
        !state.model_fs_busy() && state.model_catalog().load_confirmation().is_some()
    }));

    fs::remove_file(&link).expect("remove link");
    symlink(&second, &link).expect("repoint");
    assert!(state.request_confirm_model_load());
    assert!(wait_until(&mut state, |state| !state.model_fs_busy()));

    assert_eq!(
        state.model_catalog().error(),
        Some(&ModelPathError::Changed),
        "a changed identity must surface an actionable error"
    );
    assert!(state.model_catalog().load_confirmation().is_none());
    assert_eq!(state.model_catalog().entries().len(), 1);
    assert_eq!(
        state.model_catalog().entries()[0].file_name,
        "first-target.gguf"
    );
    assert_eq!(state.model_catalog().entries()[0].size_bytes, 4);

    // Re-registering the repointed alias refreshes the same row and clears the error.
    state
        .model_catalog_mut()
        .set_draft(link.to_str().expect("utf8"));
    assert!(state.request_register_model());
    assert!(wait_until(&mut state, |state| !state.model_fs_busy()));
    assert_eq!(state.model_catalog().entries().len(), 1);
    assert_eq!(
        state.model_catalog().entries()[0].file_name,
        "second-target.gguf"
    );
    assert!(state.model_catalog().error().is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn non_utf8_gguf_name_is_visible_lossy_text() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = scratch_dir("rawname");
    let mut bytes = dir.as_os_str().as_encoded_bytes().to_vec();
    bytes.push(b'/');
    bytes.extend_from_slice(&[0xff, 0xfe]);
    bytes.extend_from_slice(b".gguf");
    let path = PathBuf::from(OsString::from_vec(bytes));
    write_named_os(&path, b"raw");

    let registered = register_gguf_path(&path).expect("non-utf8 name");
    assert!(!registered.file_name.is_empty());
    assert!(registered.file_name.contains('\u{FFFD}'));
    assert_eq!(registered.size_bytes, 3);
    assert!(!registered.loaded);

    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
fn write_named_os(path: &Path, bytes: &[u8]) {
    let mut file = File::create(path).expect("create raw name");
    file.write_all(bytes).expect("write");
}
