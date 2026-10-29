//! Attachment selection and bookkeeping tests.

use std::collections::HashMap;

use meowland_core::pixels::FrameSize;
use meowland_panes::{Attachments, PaneId, Policy, Presentation, Selection};
use meowland_protocol::{FrameAck, FrameTransport, Show, WindowId, clipboard::Transport};
use proptest::prelude::*;

fn frame_size(width: u32, height: u32) -> FrameSize {
    FrameSize::new(width, height).expect("test dimensions are valid")
}

fn pane_id(value: u64) -> PaneId {
    PaneId::from(value)
}

fn window_id(value: u64) -> WindowId {
    WindowId::from(value)
}

#[derive(Debug)]
struct FakePresentation<const DIRTY_ON_NEW: bool, const DIRTY_ON_SIZE: bool> {
    size: FrameSize,
    transport: FrameTransport,
    due: bool,
    acked: Option<(PaneId, FrameAck)>,
}

type Fake = FakePresentation<false, false>;

impl<const DIRTY_ON_NEW: bool, const DIRTY_ON_SIZE: bool> Presentation
    for FakePresentation<DIRTY_ON_NEW, DIRTY_ON_SIZE>
{
    fn new(size: FrameSize) -> Self {
        Self {
            size,
            transport: FrameTransport::Inline,
            due: DIRTY_ON_NEW,
            acked: None,
        }
    }

    fn size(&self) -> FrameSize {
        self.size
    }

    fn set_size(&mut self, size: FrameSize) {
        self.size = size;
        if DIRTY_ON_SIZE {
            self.due = true;
        }
    }

    fn set_transport(&mut self, transport: FrameTransport) {
        self.transport = transport;
        self.due = transport == FrameTransport::Shared;
    }

    fn due(&self) -> bool {
        self.due
    }

    fn mark_dirty(&mut self) {
        self.due = true;
    }

    fn ack(&mut self, pane: PaneId, ack: FrameAck) {
        self.acked = Some((pane, ack));
        self.due = false;
    }
}

#[test]
fn insert_records_policy_and_waiting_selector() {
    let size = frame_size(80, 24);
    let cases = [
        (Show::Id(window_id(1)), None, Policy::Fixed),
        (Show::Newest, None, Policy::FollowNewest),
        (Show::Focused, None, Policy::FollowNewest),
        (Show::AppId("terminal".into()), None, Policy::Fixed),
        (Show::Activation("token".into()), None, Policy::Fixed),
        (Show::Id(window_id(1)), Some(window_id(1)), Policy::Fixed),
        (Show::Newest, Some(window_id(1)), Policy::FollowNewest),
        (Show::Focused, Some(window_id(1)), Policy::Fixed),
        (
            Show::AppId("terminal".into()),
            Some(window_id(1)),
            Policy::Fixed,
        ),
        (
            Show::Activation("token".into()),
            Some(window_id(1)),
            Policy::Fixed,
        ),
    ];

    for (index, (show, resolved, policy)) in cases.into_iter().enumerate() {
        let pane = pane_id(u64::try_from(index).unwrap());
        let mut attachments = Attachments::<Fake>::default();
        attachments.insert(pane, show.clone(), resolved, size);

        let entry = attachments.get(pane).unwrap();
        assert_eq!(entry.policy, policy, "selector {show:?}");
        assert_eq!(entry.window(), resolved, "selector {show:?}");
        assert_eq!(entry.clipboard, Transport::Kitty);
        assert_eq!(attachments.due_frames(), 0);
        match (&show, resolved) {
            (Show::AppId(app_id), None) => {
                assert!(
                    matches!(entry.waiting(), Some(Selection::AppId(waiting)) if waiting == app_id),
                    "selector {show:?}"
                );
            }
            (Show::Activation(token), None) => {
                assert!(
                    matches!(entry.waiting(), Some(Selection::Activation(waiting)) if waiting == token),
                    "selector {show:?}"
                );
            }
            _ => assert!(entry.waiting().is_none(), "selector {show:?}"),
        }
    }
}

#[test]
fn show_resolves_waiting_tokens_and_updates_the_index() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), Show::AppId("terminal".into()), None, size);
    attachments.insert(pane_id(2), Show::Activation("token".into()), None, size);
    assert_eq!(attachments.due_frames(), 0);
    assert_eq!(attachments.showing(window_id(1)), Vec::new());
    assert!(!attachments.visible(window_id(1)));

    assert_eq!(attachments.show(pane_id(1), window_id(1)), None);
    assert_eq!(attachments.due_frames(), 1);
    assert_eq!(attachments.showing(window_id(1)), vec![pane_id(1)]);
    assert_eq!(
        attachments.get(pane_id(1)).unwrap().window(),
        Some(window_id(1))
    );
    assert!(attachments.get(pane_id(1)).unwrap().waiting().is_none());

    assert_eq!(
        attachments.show(pane_id(2), window_id(2)),
        Some("token".into())
    );
    assert_eq!(attachments.due_frames(), 2);
    assert_eq!(attachments.showing(window_id(2)), vec![pane_id(2)]);

    assert_eq!(attachments.show(pane_id(1), window_id(2)), None);
    assert_eq!(attachments.due_frames(), 2);
    assert_eq!(attachments.showing(window_id(1)), Vec::new());
    assert_eq!(
        attachments.showing(window_id(2)),
        vec![pane_id(1), pane_id(2)]
    );

    assert_eq!(attachments.show(pane_id(99), window_id(1)), None);
    assert_eq!(attachments.due_frames(), 2);
}

#[test]
fn clear_keeps_the_pane_attached_and_marks_it_dirty() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), Show::Id(window_id(1)), Some(window_id(1)), size);
    assert_eq!(attachments.due_frames(), 0);

    attachments.clear(pane_id(1));
    assert_eq!(attachments.due_frames(), 1);
    assert_eq!(attachments.len(), 1);
    assert!(attachments.contains(pane_id(1)));
    assert_eq!(attachments.get(pane_id(1)).unwrap().window(), None);
    assert!(attachments.get(pane_id(1)).unwrap().waiting().is_none());
    assert!(!attachments.visible(window_id(1)));
    assert_eq!(attachments.showing(window_id(1)), Vec::new());

    attachments.clear(pane_id(1));
    assert_eq!(attachments.due_frames(), 1);

    attachments.clear(pane_id(99));
    assert_eq!(attachments.due_frames(), 1);
}

#[test]
fn remove_returns_the_pane_and_clears_its_index_entries() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), Show::Id(window_id(1)), Some(window_id(1)), size);
    attachments.insert(pane_id(2), Show::Id(window_id(1)), Some(window_id(1)), size);
    attachments.show(pane_id(2), window_id(1));
    attachments.decide(window_id(1), pane_id(2));
    assert_eq!(attachments.due_frames(), 1);
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(2)));

    let removed = attachments.remove(pane_id(2)).unwrap();
    assert_eq!(removed.window(), Some(window_id(1)));
    assert_eq!(attachments.due_frames(), 0);
    assert_eq!(attachments.showing(window_id(1)), vec![pane_id(1)]);
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(1)));

    assert_eq!(
        attachments.remove(pane_id(1)).unwrap().window(),
        Some(window_id(1))
    );
    assert_eq!(attachments.showing(window_id(1)), Vec::new());
    assert_eq!(attachments.owner(window_id(1)), None);
    assert!(attachments.remove(pane_id(1)).is_none());
}

#[test]
fn ordering_and_owner_are_deterministic() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(3), Show::Id(window_id(1)), Some(window_id(1)), size);
    attachments.insert(pane_id(1), Show::Id(window_id(2)), Some(window_id(2)), size);
    attachments.insert(pane_id(2), Show::Id(window_id(1)), Some(window_id(1)), size);

    assert_eq!(
        attachments.showing(window_id(1)),
        vec![pane_id(2), pane_id(3)]
    );
    assert_eq!(attachments.first_showing(window_id(1)), Some(pane_id(2)));
    assert_eq!(attachments.first_window(), Some(window_id(2)));
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(2)));
    assert_eq!(attachments.size(window_id(1)), Some(size));

    assert!(attachments.decide(window_id(1), pane_id(3)));
    assert!(!attachments.decide(window_id(1), pane_id(3)));
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(3)));

    attachments.resize(pane_id(3), frame_size(100, 40));
    assert_eq!(attachments.size(window_id(1)), Some(frame_size(100, 40)));

    assert!(attachments.decide(window_id(1), pane_id(1)));
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(2)));
    assert_eq!(attachments.size(window_id(1)), Some(size));

    assert!(attachments.decide(window_id(1), pane_id(99)));
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(2)));

    attachments.forget_window(window_id(1));
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(2)));
    assert_eq!(attachments.first_window(), Some(window_id(2)));
}

#[test]
fn following_lists_only_following_panes_in_identity_order() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(4), Show::Newest, None, size);
    attachments.insert(pane_id(1), Show::Newest, Some(window_id(1)), size);
    attachments.insert(pane_id(3), Show::Focused, None, size);
    attachments.insert(pane_id(2), Show::Id(window_id(1)), Some(window_id(1)), size);
    attachments.insert(pane_id(5), Show::Focused, Some(window_id(2)), size);

    assert_eq!(
        attachments.following(),
        vec![pane_id(1), pane_id(3), pane_id(4)]
    );
    assert_eq!(attachments.first_window(), Some(window_id(1)));
    assert_eq!(attachments.showing(window_id(2)), vec![pane_id(5)]);
}

#[test]
fn resize_only_dirties_when_the_size_changes() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), Show::Id(window_id(1)), Some(window_id(1)), size);

    assert_eq!(attachments.resize(pane_id(1), size), None);
    assert_eq!(attachments.due_frames(), 0);

    assert_eq!(
        attachments.resize(pane_id(1), frame_size(100, 40)),
        Some(window_id(1))
    );
    assert_eq!(attachments.due_frames(), 1);
    assert_eq!(
        attachments.get(pane_id(1)).unwrap().presentation.size(),
        frame_size(100, 40)
    );

    assert_eq!(attachments.resize(pane_id(1), frame_size(100, 40)), None);
    assert_eq!(attachments.due_frames(), 1);
    assert_eq!(attachments.resize(pane_id(99), frame_size(10, 10)), None);
}

#[test]
fn transport_and_clipboard_updates_track_dirty_state() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), Show::Id(window_id(1)), Some(window_id(1)), size);

    attachments.set_transport(pane_id(1), FrameTransport::Shared);
    assert_eq!(attachments.due_frames(), 1);
    assert_eq!(
        attachments.get(pane_id(1)).unwrap().presentation.transport,
        FrameTransport::Shared
    );

    attachments.set_transport(pane_id(1), FrameTransport::Inline);
    assert_eq!(attachments.due_frames(), 0);
    attachments.set_transport(pane_id(99), FrameTransport::Shared);
    assert_eq!(attachments.due_frames(), 0);

    assert_eq!(
        attachments.get(pane_id(1)).unwrap().clipboard,
        Transport::Kitty
    );
    attachments.set_clipboard_transport(pane_id(1), Transport::Osc52);
    assert_eq!(
        attachments.get(pane_id(1)).unwrap().clipboard,
        Transport::Osc52
    );
    attachments.set_clipboard_transport(pane_id(99), Transport::Osc52);
}

#[test]
fn acknowledgements_clear_the_due_state() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), Show::Id(window_id(1)), Some(window_id(1)), size);
    attachments.show(pane_id(1), window_id(1));
    assert_eq!(attachments.due_frames(), 1);

    attachments.ack(pane_id(1), FrameAck::Resync);
    assert_eq!(attachments.due_frames(), 0);
    assert_eq!(
        attachments.get(pane_id(1)).unwrap().presentation.acked,
        Some((pane_id(1), FrameAck::Resync))
    );

    attachments.ack(pane_id(1), FrameAck::Retained);
    assert_eq!(attachments.due_frames(), 0);
    attachments.ack(pane_id(99), FrameAck::Retained);
    assert_eq!(attachments.due_frames(), 0);
}

#[test]
fn presentation_visitors_keep_the_due_count_exact() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), Show::Id(window_id(1)), Some(window_id(1)), size);
    attachments.insert(pane_id(2), Show::Id(window_id(2)), Some(window_id(2)), size);
    attachments.insert(pane_id(3), Show::Newest, None, size);

    let mut seen = Vec::new();
    attachments.for_each_presentation_mut(|pane, window, presentation| {
        seen.push((pane, window));
        presentation.mark_dirty();
    });
    assert_eq!(
        seen,
        vec![
            (pane_id(1), Some(window_id(1))),
            (pane_id(2), Some(window_id(2))),
            (pane_id(3), None),
        ]
    );
    assert_eq!(attachments.due_frames(), 3);

    attachments.for_each_presentation_mut(|pane, _, presentation| {
        presentation.ack(pane, FrameAck::Retained);
    });
    assert_eq!(attachments.due_frames(), 0);

    let mut seen = Vec::new();
    attachments.for_each_showing(window_id(1), |pane, _| seen.push(pane));
    assert_eq!(seen, vec![pane_id(1)]);

    attachments.for_each_showing_mut(window_id(1), |pane, presentation| {
        assert_eq!(pane, pane_id(1));
        presentation.mark_dirty();
    });
    assert_eq!(attachments.due_frames(), 1);

    attachments.for_each_showing(window_id(99), |_, _| panic!("no pane shows the window"));
}

#[test]
fn insert_replaces_an_existing_attachment() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), Show::Id(window_id(1)), Some(window_id(1)), size);
    attachments.show(pane_id(1), window_id(1));
    assert_eq!(attachments.due_frames(), 1);

    attachments.insert(pane_id(1), Show::Id(window_id(2)), Some(window_id(2)), size);
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments.due_frames(), 0);
    assert_eq!(attachments.showing(window_id(1)), Vec::new());
    assert_eq!(attachments.showing(window_id(2)), vec![pane_id(1)]);

    attachments.insert(pane_id(1), Show::AppId("terminal".into()), None, size);
    assert_eq!(attachments.showing(window_id(2)), Vec::new());
    assert!(matches!(
        attachments.get(pane_id(1)).unwrap().waiting(),
        Some(Selection::AppId(app_id)) if app_id == "terminal"
    ));

    attachments.insert(pane_id(1), Show::Id(window_id(3)), Some(window_id(3)), size);
    assert_eq!(attachments.showing(window_id(3)), vec![pane_id(1)]);
}

#[derive(Clone, Debug)]
enum VisitAction {
    MarkDirty,
    Ack(FrameAck),
    SetSize(FrameSize),
}

#[derive(Clone, Debug)]
enum Operation {
    Insert {
        pane: PaneId,
        show: Show,
        resolved: Option<WindowId>,
        size: FrameSize,
    },
    Show {
        pane: PaneId,
        window: WindowId,
    },
    Clear {
        pane: PaneId,
    },
    Remove {
        pane: PaneId,
    },
    Resize {
        pane: PaneId,
        size: FrameSize,
    },
    Ack {
        pane: PaneId,
        ack: FrameAck,
    },
    SetTransport {
        pane: PaneId,
        transport: FrameTransport,
    },
    Decide {
        window: WindowId,
        pane: PaneId,
    },
    ForgetWindow {
        window: WindowId,
    },
    VisitAll {
        action: VisitAction,
    },
}

fn pane_strategy() -> impl Strategy<Value = PaneId> {
    (0u64..4).prop_map(pane_id)
}

fn window_strategy() -> impl Strategy<Value = WindowId> {
    (1u64..5).prop_map(window_id)
}

fn size_strategy() -> impl Strategy<Value = FrameSize> {
    prop_oneof![Just(frame_size(80, 24)), Just(frame_size(100, 40))]
}

fn app_id_strategy() -> impl Strategy<Value = String> {
    prop_oneof![Just("alpha".to_owned()), Just("beta".to_owned())]
}

fn token_strategy() -> impl Strategy<Value = String> {
    prop_oneof![Just("first".to_owned()), Just("second".to_owned())]
}

fn show_strategy() -> impl Strategy<Value = Show> {
    prop_oneof![
        window_strategy().prop_map(Show::Id),
        Just(Show::Newest),
        Just(Show::Focused),
        app_id_strategy().prop_map(Show::AppId),
        token_strategy().prop_map(Show::Activation),
    ]
}

fn ack_strategy() -> impl Strategy<Value = FrameAck> {
    prop_oneof![Just(FrameAck::Retained), Just(FrameAck::Resync)]
}

fn transport_strategy() -> impl Strategy<Value = FrameTransport> {
    prop_oneof![Just(FrameTransport::Inline), Just(FrameTransport::Shared)]
}

fn visit_strategy() -> impl Strategy<Value = VisitAction> {
    prop_oneof![
        Just(VisitAction::MarkDirty),
        ack_strategy().prop_map(VisitAction::Ack),
        size_strategy().prop_map(VisitAction::SetSize),
    ]
}

fn operation_strategy() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (
            pane_strategy(),
            show_strategy(),
            proptest::option::of(window_strategy()),
            size_strategy(),
        )
            .prop_map(|(pane, show, resolved, size)| Operation::Insert {
                pane,
                show,
                resolved,
                size,
            }),
        (pane_strategy(), window_strategy())
            .prop_map(|(pane, window)| Operation::Show { pane, window }),
        pane_strategy().prop_map(|pane| Operation::Clear { pane }),
        pane_strategy().prop_map(|pane| Operation::Remove { pane }),
        (pane_strategy(), size_strategy())
            .prop_map(|(pane, size)| Operation::Resize { pane, size }),
        (pane_strategy(), ack_strategy()).prop_map(|(pane, ack)| Operation::Ack { pane, ack }),
        (pane_strategy(), transport_strategy())
            .prop_map(|(pane, transport)| { Operation::SetTransport { pane, transport } }),
        (window_strategy(), pane_strategy())
            .prop_map(|(window, pane)| Operation::Decide { window, pane }),
        window_strategy().prop_map(|window| Operation::ForgetWindow { window }),
        visit_strategy().prop_map(|action| Operation::VisitAll { action }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        ..ProptestConfig::default()
    })]

    #[test]
    fn indexes_and_due_counts_stay_consistent(
        operations in prop::collection::vec(operation_strategy(), 0..200)
    ) {
        let mut attachments = Attachments::<FakePresentation<true, true>>::default();
        let mut bound = HashMap::new();
        let mut sizes = HashMap::new();
        let mut dirty = HashMap::new();
        let mut deciding = HashMap::new();

        for (step, operation) in operations.iter().enumerate() {
            match operation {
                Operation::Insert {
                    pane,
                    show,
                    resolved,
                    size,
                } => {
                    attachments.insert(*pane, show.clone(), *resolved, *size);
                    bound.insert(*pane, *resolved);
                    sizes.insert(*pane, *size);
                    dirty.insert(*pane, true);
                }
                Operation::Show { pane, window } => {
                    attachments.show(*pane, *window);
                    if let Some(binding) = bound.get_mut(pane) {
                        *binding = Some(*window);
                        dirty.insert(*pane, true);
                    }
                }
                Operation::Clear { pane } => {
                    attachments.clear(*pane);
                    if let Some(binding) = bound.get_mut(pane) {
                        *binding = None;
                        dirty.insert(*pane, true);
                    }
                }
                Operation::Remove { pane } => {
                    attachments.remove(*pane);
                    bound.remove(pane);
                    sizes.remove(pane);
                    dirty.remove(pane);
                    deciding.retain(|_, owner| *owner != *pane);
                }
                Operation::Resize { pane, size } => {
                    attachments.resize(*pane, *size);
                    if sizes.get(pane) != Some(size) && sizes.contains_key(pane) {
                        sizes.insert(*pane, *size);
                        dirty.insert(*pane, true);
                    }
                }
                Operation::Ack { pane, ack } => {
                    attachments.ack(*pane, *ack);
                    if bound.contains_key(pane) {
                        dirty.insert(*pane, false);
                    }
                }
                Operation::SetTransport { pane, transport } => {
                    attachments.set_transport(*pane, *transport);
                    if bound.contains_key(pane) {
                        dirty.insert(*pane, *transport == FrameTransport::Shared);
                    }
                }
                Operation::Decide { window, pane } => {
                    attachments.decide(*window, *pane);
                    deciding.insert(*window, *pane);
                }
                Operation::ForgetWindow { window } => {
                    attachments.forget_window(*window);
                    deciding.remove(window);
                }
                Operation::VisitAll { action } => {
                    attachments.for_each_presentation_mut(|pane, _, presentation| {
                        match action {
                            VisitAction::MarkDirty => presentation.mark_dirty(),
                            VisitAction::Ack(ack) => presentation.ack(pane, *ack),
                            VisitAction::SetSize(size) => presentation.set_size(*size),
                        }
                    });
                    match action {
                        VisitAction::MarkDirty => {
                            for dirty in dirty.values_mut() {
                                *dirty = true;
                            }
                        }
                        VisitAction::Ack(_) => {
                            for dirty in dirty.values_mut() {
                                *dirty = false;
                            }
                        }
                        VisitAction::SetSize(size) => {
                            for dirty in dirty.values_mut() {
                                *dirty = true;
                            }
                            for current in sizes.values_mut() {
                                *current = *size;
                            }
                        }
                    }
                }
            }

            let expected_due = dirty.values().filter(|dirty| **dirty).count();
            let actual_due = attachments
                .iter()
                .filter(|(_, entry)| entry.presentation.due())
                .count();
            prop_assert_eq!(
                attachments.due_frames(),
                expected_due,
                "step {} operation {:?} cached due",
                step,
                operation
            );
            prop_assert_eq!(
                actual_due, expected_due,
                "step {} operation {:?} presentation due",
                step,
                operation
            );

            for number in 1..=4u64 {
                let window = window_id(number);
                let mut expected: Vec<PaneId> = bound
                    .iter()
                    .filter_map(|(&pane, binding)| (*binding == Some(window)).then_some(pane))
                    .collect();
                expected.sort_unstable();
                let scanned: Vec<PaneId> = attachments
                    .iter()
                    .filter_map(|(&pane, entry)| (entry.window() == Some(window)).then_some(pane))
                    .collect();
                let expected_owner = deciding
                    .get(&window)
                    .copied()
                    .filter(|pane| bound.get(pane).copied().flatten() == Some(window))
                    .or_else(|| expected.first().copied());
                let showing = attachments.showing(window);

                prop_assert_eq!(
                    &scanned,
                    &expected,
                    "step {} operation {:?} window {} scan",
                    step,
                    operation,
                    number
                );
                prop_assert_eq!(
                    showing,
                    expected,
                    "step {} operation {:?} window {} index",
                    step,
                    operation,
                    number
                );
                prop_assert_eq!(
                    attachments.owner(window),
                    expected_owner,
                    "step {} operation {:?} window {} owner",
                    step,
                    operation,
                    number
                );
            }
        }
    }
}
