//! Window catalog selection tests.

use meowland_panes::WindowCatalog;
use meowland_protocol::{Show, WindowId, WindowState};

fn id(value: u64) -> WindowId {
    WindowId::from(value)
}

fn insert(catalog: &mut WindowCatalog, app_id: &str) -> WindowId {
    catalog.insert(app_id.into(), format!("{app_id} title"))
}

#[test]
fn insert_assigns_fresh_unmapped_identities() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "first");
    let second = insert(&mut catalog, "second");

    assert_ne!(first, second);
    assert_eq!(catalog.info(first).id, first);
    assert!(!catalog.drawable(first));
    assert_eq!(catalog.newest(), None);
    assert_eq!(catalog.focused(), None);
    assert_eq!(catalog.title(first), "first title");
    let info = catalog.info(second);
    assert_eq!(info.id, second);
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
fn app_id_selection_uses_the_newest_matching_mapping() {
    let mut catalog = WindowCatalog::default();
    let older = insert(&mut catalog, "terminal");
    let newer = insert(&mut catalog, "terminal");
    let other = insert(&mut catalog, "browser");
    catalog.map(older);
    catalog.map(newer);
    catalog.map(other);

    let terminal = Show::AppId("terminal".into());
    assert_eq!(catalog.select(&terminal, None), Some(newer));

    assert!(catalog.unmap(newer));
    assert_eq!(catalog.select(&terminal, None), Some(older));

    assert!(catalog.unmap(older));
    assert_eq!(catalog.select(&terminal, None), None);
    assert_eq!(catalog.select(&Show::AppId("missing".into()), None), None);
}

#[test]
fn selection_resolves_each_selector_to_a_drawable_window() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "first");
    let second = insert(&mut catalog, "second");
    catalog.map(first);
    catalog.map(second);

    assert_eq!(catalog.select(&Show::Id(first), None), Some(first));
    assert_eq!(catalog.select(&Show::Newest, None), Some(second));
    assert_eq!(catalog.select(&Show::Focused, None), None);

    catalog.focus(Some(first));
    assert_eq!(catalog.select(&Show::Focused, None), Some(first));

    assert_eq!(
        catalog.select(&Show::Activation("token".into()), Some(second)),
        Some(second)
    );
    assert_eq!(
        catalog.select(&Show::Activation("token".into()), None),
        None
    );
    assert_eq!(
        catalog.select(&Show::Activation("token".into()), Some(id(99))),
        None
    );
}

#[test]
fn unmapped_and_unknown_windows_are_not_selected() {
    let mut catalog = WindowCatalog::default();
    let mapped = insert(&mut catalog, "mapped");
    let unmapped = insert(&mut catalog, "unmapped");
    catalog.map(mapped);

    assert_eq!(catalog.select(&Show::Id(unmapped), None), None);
    assert_eq!(catalog.select(&Show::Id(id(99)), None), None);
    assert_eq!(
        catalog.select(&Show::Activation("token".into()), Some(unmapped)),
        None
    );
}

#[test]
fn update_replaces_metadata_and_affects_selection() {
    let mut catalog = WindowCatalog::default();
    let window = insert(&mut catalog, "old");
    catalog.map(window);

    catalog.update(window, "new".into(), "New title".into());
    assert_eq!(catalog.info(window).app_id, "new");
    assert_eq!(catalog.title(window), "New title");
    assert_eq!(catalog.select(&Show::AppId("old".into()), None), None);
    assert_eq!(
        catalog.select(&Show::AppId("new".into()), None),
        Some(window)
    );

    catalog.update(id(99), "missing".into(), "missing".into());
    assert_eq!(catalog.title(id(99)), "");
}

#[test]
fn remove_forgets_identity_metadata_and_drawability() {
    let mut catalog = WindowCatalog::default();
    let first = insert(&mut catalog, "first");
    let second = insert(&mut catalog, "second");
    catalog.map(first);
    catalog.map(second);
    catalog.focus(Some(second));

    catalog.remove(second);
    assert!(!catalog.drawable(second));
    assert_eq!(catalog.newest(), Some(first));
    assert_eq!(catalog.focused(), None);
    assert_eq!(catalog.select(&Show::Id(second), None), None);
    assert_eq!(catalog.title(second), "");
    assert_eq!(catalog.info(second).app_id, "");
    assert_eq!(catalog.info(second).state, WindowState::Inactive);

    catalog.remove(id(99));
}
