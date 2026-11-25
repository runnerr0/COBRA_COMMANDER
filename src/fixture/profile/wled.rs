//! Control profile for WLED via http/json API.

use std::{num::NonZeroU8, time::Duration};

use crate::{
    color::{Hsv, RenderColor},
    fixture::prelude::*,
    strobe::FlashState,
    wled::{WledControlMessage, WledController, WledUrl},
};
use wled_json_api_library::structures::state::{Seg, State};

/// The effects offered on the control surface, by WLED effect ID.
///
/// WLED has far more than fit on a control surface. The first nine advance a
/// stored phase each frame, so riding Speed bends their motion. The last three
/// derive position from the clock, as most WLED effects do, and jump when Speed
/// changes; they are here because they are the looks people reach for.
const EFFECTS: [(&str, u8); 12] = [
    ("Solid", 0),
    ("Sine", 108),
    ("Phased", 105),
    ("Noise", 70),
    ("Aurora", 38),
    ("Ripple", 79),
    ("Rain", 43),
    ("Dissolve", 18),
    ("Candle", 88),
    ("Rainbow", 9),
    ("Chase", 28),
    ("Fire", 66),
];

/// The palettes offered on the control surface, by WLED palette ID.
///
/// Color and Gradient are built from the colour set by Hue and Sat; Default
/// leaves the choice to the effect.
const PALETTES: [(&str, u8); 10] = [
    ("Default", 0),
    ("Color", 2),
    ("Gradient", 4),
    ("Party", 6),
    ("Lava", 8),
    ("Ocean", 9),
    ("Forest", 10),
    ("Rainbow", 11),
    ("Sunset", 13),
    ("Fire", 35),
];

#[derive(Debug, EmitState, Control, DescribeControls)]
pub struct Wled {
    #[channel_control]
    #[on_change = "update_level"]
    level: ChannelLevelUnipolar<Unipolar<()>>,
    #[channel_control]
    #[on_change = "update_speed"]
    speed: ChannelKnobUnipolar<Unipolar<()>>,
    #[channel_control]
    #[on_change = "update_effect_intensity"]
    size: ChannelKnobUnipolar<Unipolar<()>>,
    #[on_change = "update_preset"]
    preset: IndexedSelect<()>,
    #[on_change = "update_look"]
    effect: LabeledSelect,
    #[on_change = "update_look"]
    palette: LabeledSelect,
    /// The effect's primary colour.
    #[on_change = "update_look"]
    hue: PhaseControl<()>,
    #[on_change = "update_look"]
    sat: Unipolar<()>,
    /// Let pixel ports drive this node. When off they stand down and the node
    /// shows its own effect.
    #[on_change = "update_pixels"]
    pixels: Bool<()>,
    #[skip_control]
    #[skip_emit]
    controller: WledController,
    /// The flash in progress when the group is strobing.
    ///
    /// A WLED fixture has no DMX channels, so the group never renders it and
    /// it has to time its own flashes.
    #[skip_control]
    #[skip_emit]
    flash: FlashState,
}

#[derive(Deserialize, OptionsMenu)]
#[serde(deny_unknown_fields)]
pub struct GroupOptions {
    pub url: WledUrl,
    pub preset_count: NonZeroU8,
}

impl PatchFixture for Wled {
    const NAME: FixtureType = FixtureType("Wled");
    type GroupOptions = GroupOptions;
    type PatchOptions = NoOptions;

    fn new(options: Self::GroupOptions) -> Self {
        Self {
            level: Unipolar::new("Level", ()).with_channel_level(),
            speed: Unipolar::new("Speed", ()).with_channel_knob(0),
            size: Unipolar::new("Size", ()).with_channel_knob(1),
            preset: IndexedSelect::new(
                "Preset",
                usize::from(options.preset_count.get()),
                false,
                (),
            ),
            effect: LabeledSelect::new("Effect", 0, EFFECTS.to_vec()),
            palette: LabeledSelect::new("Palette", 0, PALETTES.to_vec()),
            hue: PhaseControl::new("Hue", ()).at_half(),
            sat: Unipolar::new("Sat", ()).at_full(),
            pixels: Bool::new_on("Pixels", ()),
            controller: WledController::run(options.url.0),
            flash: FlashState::new(FLASH_RESPONSE),
        }
    }

    fn can_strobe() -> Option<StrobeResponse> {
        Some(FLASH_RESPONSE)
    }

    fn new_patch(_: Self::GroupOptions, _: Self::PatchOptions) -> PatchConfig {
        PatchConfig {
            channel_count: 0,
            render_mode: None,
        }
    }
}

register_patcher!(Wled);
register_touchosc_template!(Wled);

/// A flash has to cross the network before it shows, so hold it long enough
/// to survive a late or lost packet.
const FLASH_RESPONSE: StrobeResponse = StrobeResponse::Long;

impl Update for Wled {
    fn update(&mut self, update: FixtureGroupUpdate, _dt: Duration) {
        let was_on = self.flash.is_on();
        if update.flash_now {
            self.flash.flash_now();
        } else {
            self.flash.update(1);
        }
        if self.flash.is_on() {
            let level = unipolar_to_range(0, 255, update.master_controls.strobe().intensity());
            self.controller.flash(level);
        } else if was_on {
            self.controller.end_flash();
        }
    }
}

impl NonAnimatedFixture for Wled {
    fn render(&self, _: &FixtureGroupControls, _: &mut [u8]) {}
}

impl Wled {
    fn set_level(&self, state: &mut State) {
        let level = unipolar_to_range(0, 255, self.level.control.val());
        if level == 0 {
            state.on = Some(false);
        } else {
            state.on = Some(true);
            state.bri = Some(level);
        };
    }

    fn set_speed(&self, state: &mut State) {
        get_seg(state).sx = Some(unipolar_to_range(0, 255, self.speed.control.val()));
    }

    fn set_size(&self, state: &mut State) {
        get_seg(state).ix = Some(unipolar_to_range(0, 255, self.size.control.val()))
    }

    fn update_level(&self, _emitter: &FixtureStateEmitter) {
        let mut state = State::default();
        self.set_level(&mut state);
        self.set_speed(&mut state);
        self.set_size(&mut state);
        self.controller.send(WledControlMessage::SetState(state));
    }

    fn update_speed(&self, _emitter: &FixtureStateEmitter) {
        let mut state = State::default();
        self.set_level(&mut state);
        self.set_speed(&mut state);
        self.set_size(&mut state);
        self.controller.send(WledControlMessage::SetState(state));
    }

    fn update_effect_intensity(&self, _emitter: &FixtureStateEmitter) {
        let mut state = State::default();
        self.set_level(&mut state);
        self.set_speed(&mut state);
        self.set_size(&mut state);
        self.controller.send(WledControlMessage::SetState(state));
    }

    /// Set the effect, its palette and its colour.
    ///
    /// These go out only when one of them changes, not with every fader move,
    /// so that a preset keeps its own look until the operator picks another.
    fn set_look(&self, state: &mut State) {
        let color = Hsv {
            hue: self.hue.val(),
            sat: self.sat.val(),
            val: UnipolarFloat::ONE,
        };
        let seg = get_seg(state);
        seg.fx = Some(u16::from(self.effect.dmx_val()));
        seg.pal = Some(u16::from(self.palette.dmx_val()));
        seg.col = Some(vec![color.rgb().to_vec()]);
    }

    fn update_look(&self, _emitter: &FixtureStateEmitter) {
        let mut state = State::default();
        self.set_level(&mut state);
        self.set_speed(&mut state);
        self.set_size(&mut state);
        self.set_look(&mut state);
        self.controller.send(WledControlMessage::SetState(state));
    }

    fn update_pixels(&self, _emitter: &FixtureStateEmitter) {
        self.controller.allow_pixels(self.pixels.val());
    }

    fn update_preset(&self, _emitter: &FixtureStateEmitter) {
        // WLED numbers presets from 1.
        let preset_id = self.preset.selected().saturating_add(1);
        let mut state = State {
            ps: Some(i32::try_from(preset_id).unwrap_or(i32::MAX)),
            ..Default::default()
        };
        // TODO: this may not actually do anything
        self.set_speed(&mut state);
        self.set_size(&mut state);
        self.controller.send(WledControlMessage::SetState(state));
    }
}

fn get_seg(state: &mut State) -> &mut Seg {
    let seg = state.seg.get_or_insert(vec![]);
    if seg.is_empty() {
        seg.push(Default::default());
    }
    &mut seg[0]
}

#[cfg(test)]
mod tests {
    use rosc::{OscMessage, OscType};

    use crate::channel::mock::no_op_emitter;
    use crate::config::{GroupId, GroupName, Options};
    use crate::fixture::patch::PATCHERS;
    use crate::osc::{OscClientId, OscControlMessage};
    use crate::wled::fake::FakeWled;

    /// Picking an effect on the control surface sends the node that effect
    /// along with the palette and colour the other controls are set to.
    #[test]
    fn picking_an_effect_sends_the_whole_look() {
        let fake = FakeWled::start();
        let patcher = PATCHERS.iter().find(|p| p.name.0 == "Wled").unwrap();
        let options = Options::from_entries([
            ("url".to_string(), fake.url.as_str().into()),
            ("preset_count".to_string(), 3.into()),
        ]);
        let mut group =
            (patcher.create_group)(GroupId::new(), GroupName("wled".to_string()), options).unwrap();

        let pick_sine = OscControlMessage::new(
            OscMessage {
                addr: "/wled/Effect/Sine".to_string(),
                args: vec![OscType::Float(1.0)],
            },
            OscClientId::example(),
        )
        .unwrap();
        group.control(&pick_sine, no_op_emitter()).unwrap();

        let seg = fake.next_state().seg.unwrap().remove(0);
        assert_eq!(seg.fx, Some(108));
        assert_eq!(seg.pal, Some(0));
        // Hue at its midpoint and full saturation.
        assert_eq!(seg.col.unwrap().len(), 1);
    }
}

pub mod rug_doctor {
    //! Composite fixture, controlling both a WLED node and an Astera controller.
    use log::{debug, error};
    use serde::Serialize;

    use crate::color::ColorRgb;

    use super::*;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct GroupOptions {
        #[serde(flatten)]
        pub wled: super::GroupOptions,
        pub presets: Vec<AsteraPreset>,
    }

    impl crate::fixture::patch::OptionsMenu for GroupOptions {
        fn menu() -> Vec<(String, crate::fixture::patch::PatchOption)> {
            super::GroupOptions::menu()
        }
    }

    #[derive(Debug, EmitState, Control, DescribeControls, Update)]
    pub struct RugDoctor {
        #[channel_control]
        wled: Wled,

        #[skip_emit]
        #[skip_control]
        presets: Vec<AsteraPreset>,
    }

    impl PatchFixture for RugDoctor {
        const NAME: FixtureType = FixtureType("RugDoctor");
        type GroupOptions = GroupOptions;
        type PatchOptions = NoOptions;

        fn new(options: Self::GroupOptions) -> Self {
            if options.presets.len() != usize::from(options.wled.preset_count.get()) {
                error!(
                    "WLED has {} presets but only {} are defined for astera",
                    options.wled.preset_count,
                    options.presets.len(),
                );
            }
            Self {
                presets: options.presets,
                wled: Wled::new(options.wled),
            }
        }

        fn new_patch(_: Self::GroupOptions, _: Self::PatchOptions) -> PatchConfig {
            PatchConfig {
                channel_count: 20,
                render_mode: None,
            }
        }
    }

    register_patcher!(RugDoctor);

    const FADE: u8 = 100;

    impl NonAnimatedFixture for RugDoctor {
        fn render(&self, _: &FixtureGroupControls, dmx_buf: &mut [u8]) {
            let preset_index = self.wled.preset.selected();
            let preset = self.presets.get(preset_index).unwrap_or_else(|| {
                debug!(
                    "selected WLED preset {preset_index} out of range for astera, using default"
                );
                &MISSING_PRESET
            });
            dmx_buf[0] = unipolar_to_range(
                0,
                255,
                UnipolarFloat::new(self.wled.level.control.val().val() * preset.level_scale),
            );
            dmx_buf[1] = 0; // strobe off
            dmx_buf[2] = preset.program_dmx_val;
            dmx_buf[3] = unipolar_to_range(
                0,
                255,
                UnipolarFloat::new(self.wled.speed.control.val().val() * preset.speed_scale),
            );
            dmx_buf[4] = FADE;
            dmx_buf[5] = 0; // pattern forward, pattern loops
            dmx_buf[6] = 0;
            dmx_buf[7] = 0; // send on modify
            // write palette
            for (i, color) in preset.colors.iter().enumerate() {
                let start = 8 + i * 3;
                dmx_buf[start] = color[0];
                dmx_buf[start + 1] = color[1];
                dmx_buf[start + 2] = color[2];
            }
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct AsteraPreset {
        pub program_dmx_val: u8,
        pub level_scale: f64,
        pub speed_scale: f64,
        pub colors: [ColorRgb; 4],
    }

    // FIXME workaround
    impl AsPatchOption for Vec<AsteraPreset> {
        fn as_patch_option() -> crate::fixture::patch::PatchOption {
            crate::fixture::patch::PatchOption::Bool
        }
    }

    static MISSING_PRESET: AsteraPreset = AsteraPreset {
        program_dmx_val: 0,
        level_scale: 1.0,
        speed_scale: 1.0,
        colors: [[0, 0, 0]; 4],
    };
}
