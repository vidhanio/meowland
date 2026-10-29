//! TOML settings deserialization and shortcut parsing.

use meowland_compositor::Settings;
use meowland_protocol::{KeyBinding, Modifiers};

fn parse(source: &str) -> Settings {
    toml::from_str(source).unwrap_or_else(|error| panic!("{source:?}: {error}"))
}

fn rejected(source: &str) -> bool {
    toml::from_str::<Settings>(source).is_err()
}

fn binding(key: char, modifiers: Modifiers) -> KeyBinding {
    KeyBinding {
        key: u32::from(key),
        modifiers,
    }
}

#[test]
fn empty_configuration_matches_the_settings_defaults() {
    let settings = parse("");
    let defaults = Settings::default();

    assert_eq!(settings.output_size, defaults.output_size);
    assert_eq!(settings.scale, defaults.scale);
    assert_eq!(settings.refresh_rate, defaults.refresh_rate);
    assert_eq!(settings.keyboard.layout, defaults.keyboard.layout);
    assert_eq!(settings.keyboard.variant, defaults.keyboard.variant);
    assert_eq!(settings.keyboard.options, defaults.keyboard.options);
    assert_eq!(
        settings.keyboard.repeat_delay,
        defaults.keyboard.repeat_delay
    );
    assert_eq!(settings.keyboard.repeat_rate, defaults.keyboard.repeat_rate);
    assert_eq!(settings.shortcuts, defaults.shortcuts);
}

#[test]
fn partial_sections_keep_the_other_defaults() {
    let settings = parse("[output]\nwidth = 800\n[keyboard]\nlayout = \"de\"\n");
    let defaults = Settings::default();

    assert_eq!(settings.output_size.width(), 800);
    assert_eq!(settings.keyboard.layout, "de");
    assert_eq!(settings.output_size.height(), defaults.output_size.height());
    assert_eq!(settings.scale, defaults.scale);
    assert_eq!(settings.refresh_rate, defaults.refresh_rate);
    assert_eq!(settings.keyboard.variant, defaults.keyboard.variant);
    assert_eq!(settings.keyboard.options, defaults.keyboard.options);
    assert_eq!(
        settings.keyboard.repeat_delay,
        defaults.keyboard.repeat_delay
    );
    assert_eq!(settings.keyboard.repeat_rate, defaults.keyboard.repeat_rate);
    assert_eq!(settings.shortcuts, defaults.shortcuts);
}

#[test]
fn explicit_values_are_validated_and_converted() {
    let settings = parse(
        "[output]\nwidth = 1280\nheight = 720\nscale = 1.5\nrefresh_rate = 60\n\
         [keyboard]\nrepeat_delay = 0\nrepeat_rate = 40\n",
    );

    assert_eq!(settings.output_size.width(), 1280);
    assert_eq!(settings.output_size.height(), 720);
    assert!((settings.scale.factor() - 1.5).abs() < f64::EPSILON);
    assert_eq!(settings.refresh_rate.millihertz(), 60_000);
    assert_eq!(settings.keyboard.repeat_delay.milliseconds(), 0);
    assert_eq!(settings.keyboard.repeat_rate.hertz(), 40);
}

#[test]
fn invalid_output_values_are_rejected() {
    let cases = [
        "[output]\nwidth = 0\n",
        "[output]\nheight = 0\n",
        "[output]\nwidth = 8193\n",
        "[output]\nwidth = 4001\nheight = 4000\n",
        "[output]\nwidth = -1\n",
        "[output]\nscale = 0.4\n",
        "[output]\nscale = 4.1\n",
        "[output]\nscale = nan\n",
        "[output]\nscale = inf\n",
        "[output]\nrefresh_rate = 0\n",
        "[output]\nrefresh_rate = 1001\n",
    ];

    for source in cases {
        assert!(rejected(source), "{source:?}");
    }
}

#[test]
fn invalid_keyboard_values_are_rejected() {
    for source in [
        "[keyboard]\nrepeat_delay = -1\n",
        "[keyboard]\nrepeat_rate = -1\n",
    ] {
        assert!(rejected(source), "{source:?}");
    }
}

#[test]
fn unknown_keys_are_rejected_at_every_level() {
    for source in [
        "unknown = true\n",
        "[output]\nunknown = true\n",
        "[keyboard]\nunknown = true\n",
        "[shortcuts]\nunknown = \"alt+x\"\n",
    ] {
        assert!(rejected(source), "{source:?}");
    }
}

#[test]
fn a_present_shortcut_table_disables_omitted_bindings() {
    let absent = parse("");
    assert!(absent.shortcuts.close_window.is_some());
    assert!(absent.shortcuts.detach_pane.is_some());

    let empty = parse("[shortcuts]\n");
    assert_eq!(empty.shortcuts.close_window, None);
    assert_eq!(empty.shortcuts.detach_pane, None);

    let only_close = parse("[shortcuts]\nclose_window = \"alt+q\"\n");
    assert_eq!(
        only_close.shortcuts.close_window,
        Some(binding('q', Modifiers::ALT))
    );
    assert_eq!(only_close.shortcuts.detach_pane, None);

    let disabled = parse("[shortcuts]\nclose_window = \"\"\ndetach_pane = \"   \"\n");
    assert_eq!(disabled.shortcuts.close_window, None);
    assert_eq!(disabled.shortcuts.detach_pane, None);
}

#[test]
fn shortcut_strings_parse_modifiers_and_keys() {
    let settings = parse(
        "[shortcuts]\nclose_window = \" CTRL + Shift + Q \"\n\
         detach_pane = \"control+alt+super+hyper+meta+x\"\n",
    );

    assert_eq!(
        settings.shortcuts.close_window,
        Some(binding('q', Modifiers::CONTROL | Modifiers::SHIFT))
    );
    assert_eq!(
        settings.shortcuts.detach_pane,
        Some(binding(
            'x',
            Modifiers::CONTROL
                | Modifiers::ALT
                | Modifiers::SUPER
                | Modifiers::HYPER
                | Modifiers::META
        ))
    );

    let unicode = parse("[shortcuts]\nclose_window = \"é\"\n");
    assert_eq!(
        unicode.shortcuts.close_window,
        Some(binding('é', Modifiers::empty()))
    );

    let bare = parse("[shortcuts]\nclose_window = \"Q\"\n");
    assert_eq!(
        bare.shortcuts.close_window,
        Some(binding('q', Modifiers::empty()))
    );
}

#[test]
fn invalid_shortcut_strings_are_rejected() {
    let cases = [
        "[shortcuts]\nclose_window = \"ctrl+\"\n",
        "[shortcuts]\nclose_window = \"ctrl++q\"\n",
        "[shortcuts]\nclose_window = \"ctrl+shift\"\n",
        "[shortcuts]\nclose_window = \"fn+x\"\n",
        "[shortcuts]\nclose_window = \"ab\"\n",
        "[shortcuts]\nclose_window = \"ctrl+\\u0001\"\n",
    ];

    for source in cases {
        assert!(rejected(source), "{source:?}");
    }
}
