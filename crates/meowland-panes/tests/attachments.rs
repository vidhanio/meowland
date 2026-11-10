//! Pane binding index tests.

use meowland_panes::{AttachedPane, Attachments, PaneId, WindowId};
use meowland_protocol::{Follow, Selector, Target};
use proptest::prelude::*;

fn pane(value: u64) -> PaneId {
    PaneId::from(value)
}

fn window(value: u64) -> WindowId {
    WindowId::from(value)
}

fn matching(app_id: &str) -> Target {
    Target::Selector(Selector::AppId(app_id.into()))
}

fn activation(token: &str) -> Target {
    Target::Activation(token.into())
}

/// Every pane's window and the window index agree in both directions.
fn assert_consistent(attachments: &Attachments<()>) {
    let mut shown = 0;
    for (id, entry) in attachments.iter() {
        if let Some(window) = entry.window() {
            assert_eq!(attachments.showing(window), Some(id), "{attachments:?}");
            shown += 1;
        }
    }
    assert_eq!(attachments.windows().count(), shown);
    for number in 0..8 {
        if let Some(id) = attachments.showing(window(number)) {
            assert_eq!(
                attachments.get(id).and_then(AttachedPane::window),
                Some(window(number))
            );
        }
    }
}

#[test]
fn a_waiting_activation_resolves_its_token_only_on_first_show() {
    let mut attachments = Attachments::default();
    attachments.insert(pane(1), activation("token"), Follow::Every, ());
    assert_eq!(
        attachments.get(pane(1)).and_then(|entry| entry.waiting()),
        Some(&activation("token"))
    );

    assert_eq!(attachments.show(pane(1), window(1)), Some("token".into()));
    assert_eq!(attachments.get(pane(1)).unwrap().waiting(), None);
    assert_eq!(attachments.show(pane(1), window(2)), None);
    assert_eq!(attachments.showing(window(1)), None);
    assert_eq!(attachments.showing(window(2)), Some(pane(1)));
    assert_consistent(&attachments);
}

#[test]
fn showing_a_window_in_another_pane_unbinds_the_first() {
    let mut attachments = Attachments::default();
    attachments.insert(pane(1), matching("app"), Follow::First, ());
    attachments.insert(pane(2), matching("app"), Follow::First, ());
    attachments.show(pane(1), window(1));

    attachments.show(pane(2), window(1));

    assert_eq!(attachments.showing(window(1)), Some(pane(2)));
    assert_eq!(
        attachments.get(pane(1)).and_then(|entry| entry.waiting()),
        Some(&matching("app"))
    );
    assert_consistent(&attachments);
}

#[test]
fn unbinding_removing_and_replacing_free_the_window() {
    let mut attachments = Attachments::default();
    attachments.insert(pane(1), matching("app"), Follow::Every, ());
    attachments.show(pane(1), window(1));
    attachments.unbind(pane(1));
    assert!(attachments.contains(pane(1)));
    assert_eq!(attachments.showing(window(1)), None);

    attachments.show(pane(1), window(2));
    attachments.insert(pane(1), matching("other"), Follow::First, ());
    assert_eq!(attachments.showing(window(2)), None);

    attachments.show(pane(1), window(3));
    assert!(attachments.remove(pane(1)).is_some());
    assert_eq!(attachments.showing(window(3)), None);
    assert!(attachments.is_empty());
    assert_consistent(&attachments);
}

#[derive(Clone, Debug)]
enum Operation {
    Insert(u64),
    Show(u64, u64),
    Unbind(u64),
    Remove(u64),
}

fn operation() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (0u64..4).prop_map(Operation::Insert),
        (0u64..4, 0u64..4).prop_map(|(pane, window)| Operation::Show(pane, window)),
        (0u64..4).prop_map(Operation::Unbind),
        (0u64..4).prop_map(Operation::Remove),
    ]
}

proptest! {
    #[test]
    fn the_window_index_stays_consistent(operations in prop::collection::vec(operation(), 0..200)) {
        let mut attachments = Attachments::default();
        for operation in operations {
            match operation {
                Operation::Insert(id) => {
                    attachments.insert(pane(id), matching("app"), Follow::Every, ());
                }
                Operation::Show(id, shown) => {
                    attachments.show(pane(id), window(shown));
                }
                Operation::Unbind(id) => attachments.unbind(pane(id)),
                Operation::Remove(id) => {
                    attachments.remove(pane(id));
                }
            }
            assert_consistent(&attachments);
        }
    }
}
