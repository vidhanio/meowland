//! Pane binding index tests.

use std::collections::{HashMap, HashSet};

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

/// Every pane's window and the window index agree in both directions, and
/// no window lists a pane twice.
fn assert_consistent(attachments: &Attachments<()>) {
    let mut listed = 0;
    for number in 0..8 {
        let shown = attachments.showing(window(number));
        let unique: HashSet<_> = shown.iter().collect();
        assert_eq!(unique.len(), shown.len(), "{attachments:?}");
        assert_eq!(attachments.owner(window(number)), shown.first().copied());
        for &id in shown {
            assert_eq!(
                attachments.get(id).and_then(AttachedPane::window),
                Some(window(number))
            );
        }
        listed += shown.len();
    }
    assert_eq!(attachments.windows().count(), listed);
}

#[test]
fn a_waiting_activation_resolves_its_token_only_on_first_show() {
    let mut attachments = Attachments::default();
    attachments.insert(pane(1), activation("token"), Follow::Every, ());
    assert_eq!(
        attachments.get(pane(1)).and_then(AttachedPane::waiting),
        Some(&activation("token"))
    );

    assert_eq!(attachments.show(pane(1), window(1)), Some("token".into()));
    assert_eq!(attachments.get(pane(1)).unwrap().waiting(), None);
    assert_eq!(attachments.show(pane(1), window(2)), None);
    assert_eq!(attachments.showing(window(1)), []);
    assert_eq!(attachments.showing(window(2)), [pane(1)]);
    assert_consistent(&attachments);
}

#[test]
fn input_moves_ownership_and_a_departing_owner_hands_it_on() {
    let mut attachments = Attachments::default();
    for id in 1..=3 {
        attachments.insert(pane(id), matching("app"), Follow::First, ());
        attachments.show(pane(id), window(1));
    }
    assert_eq!(
        attachments.owner(window(1)),
        Some(pane(1)),
        "first shown owns"
    );

    assert!(attachments.decide(window(1), pane(3)));
    assert!(!attachments.decide(window(1), pane(3)));
    assert!(!attachments.decide(window(2), pane(3)), "not shown there");
    assert_eq!(attachments.owner(window(1)), Some(pane(3)));

    attachments.insert(pane(4), matching("app"), Follow::First, ());
    attachments.show(pane(4), window(1));
    assert_eq!(
        attachments.owner(window(1)),
        Some(pane(3)),
        "showing never steals"
    );

    attachments.remove(pane(3));
    assert_eq!(
        attachments.owner(window(1)),
        Some(pane(1)),
        "the previous owner takes over"
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
    assert_eq!(attachments.showing(window(1)), []);

    attachments.show(pane(1), window(2));
    attachments.insert(pane(1), matching("other"), Follow::First, ());
    assert_eq!(attachments.showing(window(2)), []);

    attachments.show(pane(1), window(3));
    assert!(attachments.remove(pane(1)).is_some());
    assert_eq!(attachments.showing(window(3)), []);
    assert!(attachments.is_empty());
    assert_consistent(&attachments);
}

#[derive(Clone, Debug)]
enum Operation {
    Insert(u64),
    Show(u64, u64),
    Decide(u64, u64),
    Unbind(u64),
    Remove(u64),
}

fn operation() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (0u64..4).prop_map(Operation::Insert),
        (0u64..4, 0u64..4).prop_map(|(pane, window)| Operation::Show(pane, window)),
        (0u64..4, 0u64..4).prop_map(|(pane, window)| Operation::Decide(pane, window)),
        (0u64..4).prop_map(Operation::Unbind),
        (0u64..4).prop_map(Operation::Remove),
    ]
}

proptest! {
    #[test]
    fn the_index_stays_consistent_and_owners_follow_input(
        operations in prop::collection::vec(operation(), 0..200)
    ) {
        let mut attachments = Attachments::default();
        let mut claims: HashMap<WindowId, PaneId> = HashMap::new();
        for operation in operations {
            let owners: Vec<_> = (0..4).map(|id| attachments.owner(window(id))).collect();
            match operation {
                Operation::Insert(id) => {
                    attachments.insert(pane(id), matching("app"), Follow::Every, ());
                }
                Operation::Show(id, shown) => {
                    attachments.show(pane(id), window(shown));
                    if let Some(owner) = owners[usize::try_from(shown).unwrap()]
                        && owner != pane(id)
                    {
                        prop_assert_eq!(attachments.owner(window(shown)), Some(owner));
                    }
                }
                Operation::Decide(id, shown) => {
                    if attachments.showing(window(shown)).contains(&pane(id)) {
                        claims.insert(window(shown), pane(id));
                    }
                    attachments.decide(window(shown), pane(id));
                }
                Operation::Unbind(id) => attachments.unbind(pane(id)),
                Operation::Remove(id) => {
                    attachments.remove(pane(id));
                }
            }
            assert_consistent(&attachments);
            claims.retain(|&claimed, &mut claimant| {
                attachments.get(claimant).and_then(AttachedPane::window) == Some(claimed)
            });
            for (&claimed, &claimant) in &claims {
                prop_assert_eq!(
                    attachments.owner(claimed),
                    Some(claimant),
                    "the pane that last routed input still shows the window"
                );
            }
        }
    }
}
