//! Pane ownership index tests.

use std::collections::HashSet;

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

fn owned(attachments: &Attachments<()>, id: PaneId) -> Option<WindowId> {
    attachments.get(id).and_then(AttachedPane::window)
}

/// Every pane's window and the owner index agree in both directions, and no
/// window has two owners.
fn assert_consistent(attachments: &Attachments<()>) {
    let mut owned_windows = HashSet::new();
    for (id, entry) in attachments.iter() {
        if let Some(shown) = entry.window() {
            assert!(owned_windows.insert(shown), "{attachments:?}");
            assert_eq!(attachments.owner(shown), Some(id));
        }
    }
    for number in 0..8 {
        if let Some(owner) = attachments.owner(window(number)) {
            assert_eq!(owned(attachments, owner), Some(window(number)));
        }
    }
    assert_eq!(attachments.windows().count(), owned_windows.len());
}

#[test]
fn a_waiting_activation_resolves_its_token_on_show() {
    let mut attachments = Attachments::default();
    attachments.insert(pane(1), activation("token"), Follow::Every, ());
    assert_eq!(
        attachments.get(pane(1)).and_then(AttachedPane::waiting),
        Some(&activation("token"))
    );

    assert_eq!(attachments.show(pane(1), window(1)), Some("token".into()));
    assert_eq!(attachments.get(pane(1)).unwrap().waiting(), None);
    assert_eq!(attachments.show(pane(1), window(2)), None);
    assert_eq!(
        owned(&attachments, pane(1)),
        Some(window(1)),
        "an owning pane cannot take a second window"
    );
    assert_consistent(&attachments);
}

#[test]
fn an_owned_window_is_never_taken_and_frees_with_its_owner() {
    let mut attachments = Attachments::default();
    attachments.insert(pane(1), matching("app"), Follow::First, ());
    attachments.insert(pane(2), matching("app"), Follow::First, ());
    attachments.show(pane(1), window(1));
    assert!(!attachments.claimable(pane(2), window(1)));
    attachments.show(pane(2), window(1));
    assert_eq!(attachments.owner(window(1)), Some(pane(1)));
    assert_eq!(owned(&attachments, pane(2)), None);

    assert_eq!(attachments.unbind(pane(1)), Some(window(1)));
    assert_eq!(attachments.waiting().collect::<Vec<_>>(), [pane(1), pane(2)]);
    attachments.show(pane(2), window(1));
    assert_eq!(attachments.owner(window(1)), Some(pane(2)));

    attachments.insert(pane(2), matching("other"), Follow::First, ());
    assert_eq!(attachments.owner(window(1)), None, "replacing frees");
    attachments.show(pane(1), window(1));
    assert!(attachments.remove(pane(1)).is_some());
    assert_eq!(attachments.owner(window(1)), None, "removing frees");
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
    fn ownership_stays_one_to_one_and_is_never_stolen(
        operations in prop::collection::vec(operation(), 0..200)
    ) {
        let mut attachments = Attachments::default();
        for operation in operations {
            match operation {
                Operation::Insert(id) => {
                    attachments.insert(pane(id), matching("app"), Follow::Every, ());
                }
                Operation::Show(id, shown) => {
                    let before = attachments.owner(window(shown));
                    let claimable = attachments.claimable(pane(id), window(shown));
                    attachments.show(pane(id), window(shown));
                    if claimable {
                        prop_assert_eq!(attachments.owner(window(shown)), Some(pane(id)));
                    } else {
                        prop_assert_eq!(attachments.owner(window(shown)), before);
                    }
                }
                Operation::Unbind(id) => {
                    attachments.unbind(pane(id));
                }
                Operation::Remove(id) => {
                    attachments.remove(pane(id));
                }
            }
            assert_consistent(&attachments);
        }
    }
}
