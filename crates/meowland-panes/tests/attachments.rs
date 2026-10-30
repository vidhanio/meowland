//! Attachment selection and bookkeeping tests.

use std::collections::HashMap;

use meowland_core::pixels::FrameSize;
use meowland_panes::{Attachments, Matcher, PaneId, Presentation, Selection, WindowId};
use meowland_protocol::{Follow, FrameAck, FrameTransport, clipboard::Transport};
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

fn matching(app_id: &str) -> Selection {
    Selection::Matching(Matcher::ExactAppId(app_id.into()))
}

fn activation(token: &str) -> Selection {
    Selection::Activation(token.into())
}

fn waits_for_app(selection: Option<&Selection>, app_id: &str) -> bool {
    matches!(selection, Some(Selection::Matching(matcher)) if matcher.matches(None, app_id, ""))
}

fn waits_for_token(selection: Option<&Selection>, token: &str) -> bool {
    matches!(selection, Some(Selection::Activation(waiting)) if waiting == token)
}

fn shown<P: Presentation>(
    attachments: &mut Attachments<P>,
    pane: PaneId,
    selection: Selection,
    window: WindowId,
    size: FrameSize,
) {
    attachments.insert(pane, selection, Follow::First, size);
    attachments.show(pane, window);
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
fn panes_wait_on_their_selection_until_shown() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), matching("terminal"), Follow::Every, size);
    attachments.insert(pane_id(2), activation("token"), Follow::Current, size);

    for pane in [pane_id(1), pane_id(2)] {
        let entry = attachments.get(pane).unwrap();
        assert_eq!(entry.window(), None);
        assert_eq!(entry.clipboard, Transport::Kitty);
    }
    assert_eq!(attachments.get(pane_id(1)).unwrap().follow, Follow::Every);
    assert!(waits_for_app(
        attachments.get(pane_id(1)).unwrap().waiting(),
        "terminal"
    ));
    assert!(waits_for_token(
        attachments.get(pane_id(2)).unwrap().waiting(),
        "token"
    ));
    assert_eq!(attachments.due_frames(), 0);
    assert_eq!(attachments.first_window(), None);

    assert_eq!(attachments.show(pane_id(1), window_id(1)), None);
    assert!(attachments.get(pane_id(1)).unwrap().waiting().is_none());

    assert_eq!(
        attachments.show(pane_id(2), window_id(2)),
        Some("token".into())
    );
    assert!(attachments.get(pane_id(2)).unwrap().waiting().is_none());
    assert_eq!(attachments.show(pane_id(2), window_id(3)), None);
    assert_eq!(attachments.show(pane_id(2), window_id(3)), None);

    attachments.unbind(pane_id(2));
    let entry = attachments.get(pane_id(2)).unwrap();
    assert_eq!(entry.window(), None);
    assert!(waits_for_token(entry.waiting(), "token"));
    assert!(!attachments.visible(window_id(3)));
    assert_eq!(attachments.showing(window_id(3)), Vec::new());
    assert_eq!(
        attachments.show(pane_id(2), window_id(2)),
        Some("token".into())
    );

    attachments.reselect(pane_id(1), activation("other"));
    assert!(attachments.get(pane_id(1)).unwrap().waiting().is_none());
    assert!(matches!(
        attachments.get(pane_id(1)).unwrap().selection(),
        Selection::Activation(token) if token == "other"
    ));
    attachments.unbind(pane_id(1));
    assert!(waits_for_token(
        attachments.get(pane_id(1)).unwrap().waiting(),
        "other"
    ));
    assert_eq!(attachments.showing(window_id(1)), Vec::new());

    attachments.reselect(pane_id(1), matching("browser"));
    assert!(waits_for_app(
        attachments.get(pane_id(1)).unwrap().waiting(),
        "browser"
    ));
    assert!(!waits_for_app(
        attachments.get(pane_id(1)).unwrap().waiting(),
        "terminal"
    ));
    assert_eq!(attachments.show(pane_id(1), window_id(1)), None);
}

#[test]
fn show_resolves_waiting_tokens_and_updates_the_index() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    attachments.insert(pane_id(1), matching("terminal"), Follow::First, size);
    attachments.insert(pane_id(2), activation("token"), Follow::First, size);
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
fn unbind_keeps_the_pane_attached_and_marks_it_dirty() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    shown(
        &mut attachments,
        pane_id(1),
        matching("terminal"),
        window_id(1),
        size,
    );
    attachments.ack(pane_id(1), FrameAck::Retained);
    assert_eq!(attachments.due_frames(), 0);

    attachments.unbind(pane_id(1));
    assert_eq!(attachments.due_frames(), 1);
    assert_eq!(attachments.len(), 1);
    assert!(attachments.contains(pane_id(1)));
    assert_eq!(attachments.get(pane_id(1)).unwrap().window(), None);
    assert!(!attachments.visible(window_id(1)));
    assert_eq!(attachments.showing(window_id(1)), Vec::new());

    attachments.unbind(pane_id(1));
    assert_eq!(attachments.due_frames(), 1);

    attachments.unbind(pane_id(99));
    assert_eq!(attachments.due_frames(), 1);
}

#[test]
fn remove_returns_the_pane_and_clears_its_index_entries() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    shown(
        &mut attachments,
        pane_id(1),
        matching("terminal"),
        window_id(1),
        size,
    );
    shown(
        &mut attachments,
        pane_id(2),
        matching("terminal"),
        window_id(1),
        size,
    );
    attachments.decide(window_id(1), pane_id(2));
    assert_eq!(attachments.due_frames(), 2);
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(2)));

    let removed = attachments.remove(pane_id(2)).unwrap();
    assert_eq!(removed.window(), Some(window_id(1)));
    assert_eq!(attachments.due_frames(), 1);
    assert_eq!(attachments.showing(window_id(1)), vec![pane_id(1)]);
    assert_eq!(attachments.owner(window_id(1)), Some(pane_id(1)));

    assert_eq!(
        attachments.remove(pane_id(1)).unwrap().window(),
        Some(window_id(1))
    );
    assert_eq!(attachments.due_frames(), 0);
    assert_eq!(attachments.showing(window_id(1)), Vec::new());
    assert_eq!(attachments.owner(window_id(1)), None);
    assert!(attachments.remove(pane_id(1)).is_none());
}

#[test]
fn ordering_and_owner_are_deterministic() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    shown(
        &mut attachments,
        pane_id(3),
        matching("terminal"),
        window_id(1),
        size,
    );
    shown(
        &mut attachments,
        pane_id(1),
        matching("terminal"),
        window_id(2),
        size,
    );
    shown(
        &mut attachments,
        pane_id(2),
        matching("terminal"),
        window_id(1),
        size,
    );

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
fn resize_only_dirties_when_the_size_changes() {
    let size = frame_size(80, 24);
    let mut attachments = Attachments::<Fake>::default();
    shown(
        &mut attachments,
        pane_id(1),
        matching("terminal"),
        window_id(1),
        size,
    );
    attachments.ack(pane_id(1), FrameAck::Retained);

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
    shown(
        &mut attachments,
        pane_id(1),
        matching("terminal"),
        window_id(1),
        size,
    );
    attachments.ack(pane_id(1), FrameAck::Retained);

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
    shown(
        &mut attachments,
        pane_id(1),
        matching("terminal"),
        window_id(1),
        size,
    );
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
    shown(
        &mut attachments,
        pane_id(1),
        matching("terminal"),
        window_id(1),
        size,
    );
    shown(
        &mut attachments,
        pane_id(2),
        matching("terminal"),
        window_id(2),
        size,
    );
    attachments.insert(pane_id(3), matching("terminal"), Follow::Every, size);

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
    shown(
        &mut attachments,
        pane_id(1),
        activation("token"),
        window_id(1),
        size,
    );
    assert_eq!(attachments.due_frames(), 1);

    attachments.insert(pane_id(1), matching("terminal"), Follow::First, size);
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments.due_frames(), 0);
    assert_eq!(attachments.showing(window_id(1)), Vec::new());
    assert!(!attachments.visible(window_id(1)));
    assert!(waits_for_app(
        attachments.get(pane_id(1)).unwrap().waiting(),
        "terminal"
    ));

    assert_eq!(attachments.show(pane_id(1), window_id(2)), None);
    assert_eq!(attachments.showing(window_id(2)), vec![pane_id(1)]);

    attachments.insert(pane_id(1), activation("again"), Follow::First, size);
    assert_eq!(attachments.showing(window_id(2)), Vec::new());
    assert_eq!(
        attachments.show(pane_id(1), window_id(3)),
        Some("again".into())
    );
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
        selection: Selection,
        size: FrameSize,
    },
    Show {
        pane: PaneId,
        window: WindowId,
    },
    Unbind {
        pane: PaneId,
    },
    Reselect {
        pane: PaneId,
        selection: Selection,
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

fn selection_strategy() -> impl Strategy<Value = Selection> {
    prop_oneof![
        app_id_strategy().prop_map(|app_id| matching(&app_id)),
        token_strategy().prop_map(Selection::Activation),
    ]
}

fn token_of(selection: &Selection) -> Option<String> {
    match selection {
        Selection::Activation(token) => Some(token.clone()),
        Selection::Matching(_) => None,
    }
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
        (pane_strategy(), selection_strategy(), size_strategy()).prop_map(
            |(pane, selection, size)| Operation::Insert {
                pane,
                selection,
                size,
            }
        ),
        (pane_strategy(), window_strategy())
            .prop_map(|(pane, window)| Operation::Show { pane, window }),
        pane_strategy().prop_map(|pane| Operation::Unbind { pane }),
        (pane_strategy(), selection_strategy())
            .prop_map(|(pane, selection)| Operation::Reselect { pane, selection }),
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
        let mut tokens: HashMap<PaneId, Option<String>> = HashMap::new();

        for (step, operation) in operations.iter().enumerate() {
            match operation {
                Operation::Insert {
                    pane,
                    selection,
                    size,
                } => {
                    attachments.insert(*pane, selection.clone(), Follow::Every, *size);
                    bound.insert(*pane, None);
                    tokens.insert(*pane, token_of(selection));
                    sizes.insert(*pane, *size);
                    dirty.insert(*pane, true);
                }
                Operation::Show { pane, window } => {
                    let token = attachments.show(*pane, *window);
                    let expected_token = bound.get_mut(pane).and_then(|binding| {
                        let was_waiting = binding.replace(*window).is_none();
                        dirty.insert(*pane, true);
                        tokens.get(pane).cloned().flatten().filter(|_| was_waiting)
                    });
                    prop_assert_eq!(
                        token,
                        expected_token,
                        "step {} operation {:?} token",
                        step,
                        operation
                    );
                }
                Operation::Unbind { pane } => {
                    attachments.unbind(*pane);
                    if let Some(binding) = bound.get_mut(pane) {
                        *binding = None;
                        dirty.insert(*pane, true);
                    }
                }
                Operation::Reselect { pane, selection } => {
                    attachments.reselect(*pane, selection.clone());
                    if let Some(token) = tokens.get_mut(pane) {
                        *token = token_of(selection);
                    }
                }
                Operation::Remove { pane } => {
                    attachments.remove(*pane);
                    bound.remove(pane);
                    tokens.remove(pane);
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
