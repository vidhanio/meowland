//! Window catalog selection tests.

use std::collections::HashSet;

use meowland_panes::{Matcher, WindowCatalog, WindowId};
use meowland_protocol::{Selector, WindowState};

fn id(value: u64) -> WindowId {
    WindowId::from(value)
}

fn insert(catalog: &mut WindowCatalog, app_id: &str) -> WindowId {
    catalog.insert(app_id.into(), format!("{app_id} title"))
}

fn matcher(selector: Selector) -> Matcher {
    Matcher::new(selector).expect("test selectors are valid")
}

fn by_id(catalog: &WindowCatalog, window: WindowId) -> Matcher {
    matcher(Selector::Id(catalog.info(window).id))
}

fn split_unique_id(unique: &str) -> (&str, &str) {
    unique
        .rsplit_once('_')
        .expect("unique IDs separate prefix and suffix with an underscore")
}

fn assert_suffix_is_base36(unique: &str) {
    let (_, suffix) = split_unique_id(unique);
    assert_eq!(suffix.len(), 4, "suffix of {unique}");
    assert!(
        suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase()),
        "suffix of {unique}"
    );
}

#[test]
fn insert_assigns_fresh_unmapped_identities() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "first");
    let second = insert(&mut catalog, "second");

    assert_ne!(first, second);
    assert_eq!(catalog.info(first).id, "");
    assert!(!catalog.drawable(first));
    assert_eq!(catalog.newest(), None);
    assert_eq!(catalog.focused(), None);
    assert_eq!(catalog.title(first), "first title");
    assert_eq!(catalog.app_id(first), "first");
    let info = catalog.info(second);
    assert_eq!(info.id, "");
    assert_eq!(info.app_id, "second");
    assert_eq!(info.title, "second title");
    assert_eq!(info.state, WindowState::Inactive);
}

#[test]
fn mapping_tracks_the_newest_drawable_window() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "first");
    let second = insert(&mut catalog, "second");

    assert!(catalog.map(first));
    assert!(catalog.drawable(first));
    assert_eq!(catalog.newest(), Some(first));
    assert!(!catalog.map(first));
    assert_eq!(catalog.newest(), Some(first));

    assert!(catalog.map(second));
    assert_eq!(catalog.newest(), Some(second));
    assert!(!catalog.map(id(99)));
}

#[test]
fn unmapping_the_newest_falls_back_to_the_next_one() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "first");
    let second = insert(&mut catalog, "second");
    catalog.map(first);
    catalog.map(second);

    assert!(catalog.unmap(second));
    assert_eq!(catalog.newest(), Some(first));
    assert!(!catalog.drawable(second));
    assert!(catalog.unmap(first));
    assert_eq!(catalog.newest(), None);
    assert!(!catalog.unmap(first));
    assert!(!catalog.unmap(id(99)));
}

#[test]
fn unmapping_the_focused_window_clears_focus() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "first");
    let second = insert(&mut catalog, "second");
    catalog.map(first);
    catalog.map(second);

    assert_eq!(catalog.focus(Some(first)), None);
    assert_eq!(catalog.focused(), Some(first));
    assert_eq!(catalog.info(first).state, WindowState::Active);
    assert_eq!(catalog.info(second).state, WindowState::Inactive);

    assert_eq!(catalog.focus(Some(second)), Some(first));
    assert_eq!(catalog.focused(), Some(second));
    assert!(catalog.unmap(second));
    assert_eq!(catalog.focused(), None);
    assert_eq!(catalog.info(second).state, WindowState::Inactive);
}

#[test]
fn focus_ignores_windows_that_are_not_drawable() {
    let mut catalog = WindowCatalog::default();
    let mapped = insert(&mut catalog, "mapped");
    let unmapped = insert(&mut catalog, "unmapped");
    catalog.map(mapped);

    assert_eq!(catalog.focus(Some(unmapped)), None);
    assert_eq!(catalog.focused(), None);
    assert_eq!(catalog.focus(Some(mapped)), None);
    assert_eq!(catalog.focused(), Some(mapped));
    assert_eq!(catalog.focus(None), Some(mapped));
    assert_eq!(catalog.focused(), None);
}

#[test]
fn unique_id_uses_the_app_id_at_first_map() {
    let mut catalog = WindowCatalog::default();
    let window = insert(&mut catalog, "early");

    catalog.update(window, "org.example.App".into(), "title".into());
    catalog.map(window);
    let unique = catalog.info(window).id;
    assert_eq!(split_unique_id(&unique).0, "org.example.App");
    assert_suffix_is_base36(&unique);

    catalog.update(window, "renamed".into(), "title".into());
    assert_eq!(catalog.info(window).id, unique);
    assert_eq!(catalog.app_id(window), "renamed");
}

#[test]
fn unique_id_is_stable_across_unmap_and_remap() {
    let mut catalog = WindowCatalog::default();
    let window = insert(&mut catalog, "terminal");
    catalog.map(window);
    let unique = catalog.info(window).id;

    catalog.unmap(window);
    assert_eq!(catalog.info(window).id, unique);
    catalog.update(window, "other".into(), "title".into());
    catalog.map(window);
    assert_eq!(catalog.info(window).id, unique);
}

#[test]
fn unique_id_prefix_falls_back_and_sanitizes() {
    let mut catalog = WindowCatalog::default();
    let cases = [
        ("", "window"),
        ("org.example-App_2", "org.example-App_2"),
        ("my app/1:é", "my-app-1--"),
    ];

    for (app_id, prefix) in cases {
        let window = insert(&mut catalog, app_id);
        catalog.map(window);
        let unique = catalog.info(window).id;
        assert_eq!(split_unique_id(&unique).0, prefix, "app id {app_id:?}");
        assert_suffix_is_base36(&unique);
    }
}

#[test]
fn unique_ids_are_distinct_for_many_windows_with_one_app_id() {
    let mut catalog = WindowCatalog::default();
    let mut seen = HashSet::new();

    for _ in 0..2000 {
        let window = insert(&mut catalog, "terminal");
        catalog.map(window);
        let unique = catalog.info(window).id;
        assert!(seen.insert(unique.clone()), "duplicate unique ID {unique}");
    }
}

#[test]
fn id_selection_finds_only_the_drawable_window_with_that_id() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "terminal");
    let second = insert(&mut catalog, "terminal");
    catalog.map(first);
    catalog.map(second);
    let first_id = by_id(&catalog, first);

    assert_eq!(catalog.select(&first_id), Some(first));
    assert_eq!(catalog.select(&by_id(&catalog, second)), Some(second));

    catalog.unmap(first);
    assert_eq!(catalog.select(&first_id), None);
    catalog.map(first);
    assert_eq!(catalog.select(&first_id), Some(first));

    assert_eq!(catalog.select(&matcher(Selector::Id(String::new()))), None);
    assert_eq!(
        catalog.select(&matcher(Selector::Id("missing_0000".into()))),
        None
    );
}

#[test]
fn app_id_selection_uses_the_newest_matching_mapping() {
    let mut catalog = WindowCatalog::default();
    let older = insert(&mut catalog, "terminal");
    let newer = insert(&mut catalog, "terminal");
    let other = insert(&mut catalog, "browser");
    let unmapped = insert(&mut catalog, "terminal");
    catalog.map(older);
    catalog.map(newer);
    catalog.map(other);

    let terminal = matcher(Selector::AppId("term".into()));
    assert_eq!(catalog.select(&terminal), Some(newer));

    assert!(catalog.unmap(newer));
    assert_eq!(catalog.select(&terminal), Some(older));

    assert!(catalog.unmap(older));
    assert_eq!(catalog.select(&terminal), None);
    assert!(!catalog.drawable(unmapped));
    assert_eq!(
        catalog.select(&matcher(Selector::AppId("^missing$".into()))),
        None
    );
}

#[test]
fn title_selection_uses_the_newest_matching_mapping_and_follows_updates() {
    let mut catalog = WindowCatalog::default();
    let older = catalog.insert("editor".into(), "notes.txt".into());
    let newer = catalog.insert("editor".into(), "draft.txt".into());
    let hidden = catalog.insert("editor".into(), "secret.txt".into());
    catalog.map(older);
    catalog.map(newer);

    let text = matcher(Selector::Title(r"\.txt$".into()));
    assert_eq!(catalog.select(&text), Some(newer));
    let notes = matcher(Selector::Title("^notes".into()));
    assert_eq!(catalog.select(&notes), Some(older));
    assert_eq!(
        catalog.select(&matcher(Selector::Title("secret".into()))),
        None
    );
    assert!(!catalog.drawable(hidden));

    catalog.update(older, "editor".into(), "renamed.md".into());
    assert_eq!(catalog.select(&notes), None);
    catalog.update(newer, "editor".into(), "notes.md".into());
    assert_eq!(catalog.select(&notes), Some(newer));
    assert_eq!(catalog.select(&text), None);
}

#[test]
fn update_replaces_metadata_and_affects_selection() {
    let mut catalog = WindowCatalog::default();
    let window = insert(&mut catalog, "old");
    catalog.map(window);

    catalog.update(window, "new".into(), "New title".into());
    assert_eq!(catalog.info(window).app_id, "new");
    assert_eq!(catalog.title(window), "New title");
    assert_eq!(catalog.select(&Matcher::ExactAppId("old".into())), None);
    assert_eq!(
        catalog.select(&Matcher::ExactAppId("new".into())),
        Some(window)
    );

    catalog.update(id(99), "missing".into(), "missing".into());
    assert_eq!(catalog.title(id(99)), "");
    assert_eq!(catalog.app_id(id(99)), "");
}

#[test]
fn remove_forgets_identity_metadata_and_drawability() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "first");
    let second = insert(&mut catalog, "second");
    catalog.map(first);
    catalog.map(second);
    catalog.focus(Some(second));
    let second_id = by_id(&catalog, second);

    catalog.remove(second);
    assert!(!catalog.drawable(second));
    assert_eq!(catalog.newest(), Some(first));
    assert_eq!(catalog.focused(), None);
    assert_eq!(catalog.select(&second_id), None);
    assert_eq!(catalog.title(second), "");
    assert_eq!(catalog.info(second).id, "");
    assert_eq!(catalog.info(second).app_id, "");
    assert_eq!(catalog.info(second).state, WindowState::Inactive);

    catalog.remove(id(99));
}
