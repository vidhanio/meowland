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
struct Fake {
    size: FrameSize,
    transport: FrameTransport,
    due: bool,
    acked: Option<(PaneId, FrameAck)>,
}

impl Presentation for Fake {
    fn new(size: FrameSize) -> Self {
        Self {
            size,
            transport: FrameTransport::Inline,
            due: false,
            acked: None,
        }
    }

    fn size(&self) -> FrameSize {
        self.size
    }

    fn set_size(&mut self, size: FrameSize) {
        self.size = size;
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

fn decoded_show(raw: u16, window: WindowId, other: WindowId) -> Show {
    match (raw >> 7) % 8 {
        0 => Show::Id(window),
        1 => Show::Id(other),
        2 => Show::Newest,
        3 => Show::Focused,
        4 => Show::AppId("alpha".into()),
        5 => Show::AppId("beta".into()),
        6 => Show::Activation("first".into()),
        _ => Show::Activation("second".into()),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        ..ProptestConfig::default()
    })]

    #[test]
    fn indexes_and_due_counts_stay_consistent(operations in prop::collection::vec(any::<u16>(), 0..200)) {
        let sizes = [frame_size(80, 24), frame_size(100, 40)];
        let mut attachments = Attachments::<Fake>::default();
        let mut deciding = HashMap::new();

        for (step, &raw) in operations.iter().enumerate() {
            let pane = pane_id(u64::from(raw & 3));
            let window = window_id(1 + u64::from((raw >> 2) & 3));
            let other = window_id(1 + u64::from((raw >> 4) & 3));
            let size = sizes[usize::from((raw >> 6) & 1)];
            let show = decoded_show(raw, window, other);

            match (raw >> 10) % 10 {
                0 => attachments.insert(
                    pane,
                    show,
                    ((raw >> 13) & 1 == 1).then_some(window),
                    size,
                ),
                1 => {
                    attachments.show(pane, window);
                }
                2 => attachments.clear(pane),
                3 => {
                    attachments.remove(pane);
                    deciding.retain(|_, owner| *owner != pane);
                }
                4 => {
                    attachments.resize(pane, size);
                }
                5 => attachments.ack(
                    pane,
                    if raw & 1 == 0 {
                        FrameAck::Retained
                    } else {
                        FrameAck::Resync
                    },
                ),
                6 => attachments.set_transport(
                    pane,
                    if raw & 1 == 0 {
                        FrameTransport::Inline
                    } else {
                        FrameTransport::Shared
                    },
                ),
                7 => {
                    attachments.decide(window, pane);
                    deciding.insert(window, pane);
                }
                8 => {
                    attachments.forget_window(window);
                    deciding.remove(&window);
                }
                _ => attachments.for_each_presentation_mut(|_, _, presentation| {
                    if raw & 1 == 0 {
                        presentation.mark_dirty();
                    } else {
                        presentation.ack(pane, FrameAck::Retained);
                    }
                }),
            }

            let expected_due = attachments
                .iter()
                .filter(|(_, entry)| entry.presentation.due())
                .count();
            prop_assert_eq!(
                attachments.due_frames(),
                expected_due,
                "step {} operation {:#06x}",
                step,
                raw
            );

            for number in 1..=4u64 {
                let window = window_id(number);
                let expected: Vec<PaneId> = attachments
                    .iter()
                    .filter_map(|(&pane, entry)| (entry.window() == Some(window)).then_some(pane))
                    .collect();
                let expected_owner = deciding
                    .get(&window)
                    .copied()
                    .filter(|pane| {
                        attachments
                            .get(*pane)
                            .is_some_and(|entry| entry.window() == Some(window))
                    })
                    .or_else(|| expected.first().copied());
                prop_assert_eq!(
                    attachments.showing(window),
                    expected,
                    "step {} window {}",
                    step,
                    number
                );
                prop_assert_eq!(
                    attachments.owner(window),
                    expected_owner,
                    "step {} window {}",
                    step,
                    number
                );
            }

            prop_assert_eq!(
                attachments.first_window(),
                attachments.iter().find_map(|(_, entry)| entry.window()),
                "step {} operation {:#06x}",
                step,
                raw
            );
        }
    }
}
