//! # Default Keybindings
//!
//! The keybindings are set up here. We define some iamb-specific keybindings, but the default Vim
//! keys come from [modalkit::env::vim::keybindings].
use std::collections::HashSet;

use modalkit::actions::MacroAction;
use modalkit::env::CommonKeyClass;
use modalkit::env::vim::VimMode;
use modalkit::env::vim::keybindings::{InputStep, VimBindings};
use modalkit::keybindings::{BindingMachine, EdgeEvent, EdgeRepeat, InputBindings};

use crate::base::{Keybindings, MATRIX_ID_WORD};
use crate::config::Keys;
use crate::prelude::*;

pub type IambStep = InputStep<IambInfo>;

fn once(key: &TerminalKey) -> (EdgeRepeat, EdgeEvent<TerminalKey, CommonKeyClass>) {
    (EdgeRepeat::Once, EdgeEvent::Key(*key))
}

/// Initialize the default keybinding state.
pub fn setup_keybindings(tunables: &TunableValues) -> Keybindings {
    let mut ism = Keybindings::empty();

    let vim = VimBindings::default()
        .default_split(tunables.default_split.into())
        .submit_on_enter(!tunables.send_on_enter)
        .cursor_open(MATRIX_ID_WORD.clone());

    vim.setup(&mut ism);

    let ctrl_w = "<C-W>".parse::<TerminalKey>().unwrap();
    let ctrl_m = "<C-M>".parse::<TerminalKey>().unwrap();
    let ctrl_z = "<C-Z>".parse::<TerminalKey>().unwrap();
    let key_m_lc = "m".parse::<TerminalKey>().unwrap();
    let key_z_lc = "z".parse::<TerminalKey>().unwrap();
    let shift_enter = "<S-Enter>".parse::<TerminalKey>().unwrap();

    let cwz = vec![once(&ctrl_w), once(&key_z_lc)];
    let cwcz = vec![once(&ctrl_w), once(&ctrl_z)];
    let zoom = IambStep::new()
        .actions(vec![WindowAction::ZoomToggle.into()])
        .goto(VimMode::Normal);

    ism.add_mapping(VimMode::Normal, &cwz, &zoom);
    ism.add_mapping(VimMode::Visual, &cwz, &zoom);
    ism.add_mapping(VimMode::Normal, &cwcz, &zoom);
    ism.add_mapping(VimMode::Visual, &cwcz, &zoom);

    let cwm = vec![once(&ctrl_w), once(&key_m_lc)];
    let cwcm = vec![once(&ctrl_w), once(&ctrl_m)];
    let stoggle = IambStep::new()
        .actions(vec![IambAction::ToggleScrollbackFocus.into()])
        .goto(VimMode::Normal);
    ism.add_mapping(VimMode::Normal, &cwm, &stoggle);
    ism.add_mapping(VimMode::Visual, &cwm, &stoggle);
    ism.add_mapping(VimMode::Normal, &cwcm, &stoggle);
    ism.add_mapping(VimMode::Visual, &cwcm, &stoggle);

    let shift_enter = vec![once(&shift_enter)];
    let newline = IambStep::new().actions(vec![
        InsertTextAction::Type(Char::Single('\n').into(), MoveDir1D::Previous, 1.into()).into(),
    ]);
    ism.add_mapping(VimMode::Insert, &cwm, &newline);
    ism.add_mapping(VimMode::Insert, &shift_enter, &newline);

    ism
}

/// Whether a key is a complete command on its own in Normal mode, like `<Space>` or `,`, rather
/// than the start of a longer one, like `g` or `<C-W>`.
fn is_normal_command(key: &TerminalKey, tunables: &TunableValues) -> bool {
    let mut probe = setup_keybindings(tunables);
    probe.input_key(*key);
    probe.pop().is_some()
}

impl InputBindings<TerminalKey, IambStep> for ApplicationSettings {
    fn setup(&self, bindings: &mut Keybindings) {
        // A Normal mode key that's already a complete command would run straight away, so turn it
        // into a prefix when a macro starts with it, letting keys like <Space> act as a leader.
        // Keys that already start longer commands are left alone, since this would break those.
        let leaders = self
            .macros
            .iter()
            .filter(|(modes, _)| modes.0.contains(&VimMode::Normal))
            .flat_map(|(_, keys)| keys.keys())
            .filter(|Keys(input, _)| input.len() > 1)
            .map(|Keys(input, _)| input[0])
            .collect::<HashSet<_>>();

        for key in leaders {
            if is_normal_command(&key, &self.tunables) {
                // An empty step on a key with mappings after it waits for more input.
                bindings.add_mapping(VimMode::Normal, &[once(&key)], &IambStep::new());
            }
        }

        for (modes, keys) in &self.macros {
            for (Keys(input, _), Keys(_, run)) in keys {
                let act = MacroAction::Run(run.clone(), Count::Contextual);
                let step = IambStep::new().actions(vec![act.into()]);
                let input = input.iter().map(once).collect::<Vec<_>>();

                for mode in &modes.0 {
                    bindings.add_mapping(*mode, &input, &step);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::mock_settings;

    /// The actions that running a sequence of keys produces, with the given macros.
    fn run(macros: &str, input: &[&str]) -> Vec<String> {
        let mut settings = mock_settings();
        settings.macros = toml::from_str(macros).unwrap();

        let mut bindings = setup_keybindings(&settings.tunables);
        settings.setup(&mut bindings);

        for key in input {
            bindings.input_key(key.parse::<TerminalKey>().unwrap());
        }

        std::iter::from_fn(|| bindings.pop())
            .map(|(act, _)| format!("{act:?}"))
            .filter(|act| !act.contains("Checkpoint"))
            .collect()
    }

    #[test]
    fn test_leader_macros() {
        let macros = "[normal]\n\"<Space>gc\" = \":chats<Enter>\"\n\",d\" = \":dms<Enter>\"\n";
        let chats = vec![r#"Macro(Run(":chats<Enter>", Contextual))"#.to_string()];
        let dms = vec![r#"Macro(Run(":dms<Enter>", Contextual))"#.to_string()];

        assert_eq!(run(macros, &["<Space>", "g", "c"]), chats);
        assert_eq!(run(macros, &[",", "d"]), dms);

        // The keys after the leader still do what they normally do on their own.
        for keys in [vec!["g", "c"], vec!["d", "d"], vec!["w"], vec!["c", "w"]] {
            assert_eq!(run(macros, &keys), run("", &keys), "{keys:?}");
        }
        let macros = "[normal]\n\"<Space>w\" = \":whois<Enter>\"\n";
        assert_eq!(run(macros, &["w"]), run("", &["w"]));
        assert_eq!(run(macros, &["<Space>", "w"]), vec![
            r#"Macro(Run(":whois<Enter>", Contextual))"#
        ]);

        // Without the macros, <Space> moves straight away.
        assert_eq!(run("", &["<Space>"]).len(), 1);
        assert!(run("", &["<Space>"])[0].contains("Motion"));
    }

    #[test]
    fn test_prefix_macros_keep_vim_keys() {
        let macros = "[normal]\n\"gx\" = \":chats<Enter>\"\n\"<C-W>q\" = \":dms<Enter>\"\n";

        assert_eq!(run(macros, &["g", "x"]), vec![r#"Macro(Run(":chats<Enter>", Contextual))"#]);
        assert_eq!(run(macros, &["g", "g"]), run("", &["g", "g"]));
        assert_eq!(run(macros, &["<C-W>", "v"]), run("", &["<C-W>", "v"]));
        assert_eq!(run(macros, &["<C-W>", "m"]), run("", &["<C-W>", "m"]));
        assert!(!run("", &["g", "g"]).is_empty());
    }

    #[test]
    fn test_insert_macros_unchanged() {
        let macros = "[insert]\n\"jj\" = \"<Esc>\"\n";
        let mut settings = mock_settings();
        settings.macros = toml::from_str(macros).unwrap();
        let mut bindings = setup_keybindings(&settings.tunables);
        settings.setup(&mut bindings);

        // Normal mode <Space> still moves, since the macro is only for Insert mode.
        bindings.input_key("<Space>".parse::<TerminalKey>().unwrap());
        assert!(format!("{:?}", bindings.pop()).contains("Motion"));
    }
}
